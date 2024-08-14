/*
tabletop_club_lobby_server
Copyright (c) 2024 Benjamin 'drwhut' Beddows.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

use crate::close_code::CloseCode;
use crate::config::VariableConfig;
use crate::message::{LobbyCommand, LobbyRequest};

use futures_util::future;
use futures_util::sink::{Close, SinkExt};
use futures_util::stream::StreamExt;
use std::borrow::Cow;
use std::fmt;
use std::fs::read;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Duration};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, trace, warn};

/// The type used for handle IDs.
pub type HandleID = u32;

/// The type used for player IDs.
pub type PlayerID = u32;

/// A type alias for a player's WebSocket stream.
pub type PlayerStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The maximum amount of time to wait for the client to send back an echoed
/// close frame after we have sent one.
const WAIT_FOR_CLOSE_ECHO_DURATION: Duration = Duration::from_secs(5);

/// The data required to spawn a [`PlayerClose`] instance.
#[derive(Debug)]
pub struct PlayerCloseContext {
    /// The WebSocket stream for this player.
    pub client_stream: PlayerStream,

    /// The [`CloseCode`] to send to, and expect back from, the client.
    pub close_code: CloseCode,

    /// An optional [`HandleID`] for logging purposes.
    pub maybe_handle_id: Option<HandleID>,

    /// An optional [`PlayerID`] for logging purposes.
    pub maybe_player_id: Option<PlayerID>,
}

/// An instance which sends a close code to a given player, and then waits for
/// the client to echo it back, up to a given amount of time.
/// 
/// This instance should be used whenever the server wants to initialise the
/// process of closing the connection with a client. If however the client is
/// the one to initialise it, then the instance handling the client connection
/// should simply echo it back, send messages where necessary, and then drop the
/// connection out of scope.
/// 
/// Since this instance can potentially take up to a few seconds to complete,
/// it should be added to a task tracker so that connections are closed
/// gracefully when the server is shutting down.
pub struct PlayerClose {
    handle: JoinHandle<()>,
}

impl PlayerClose {
    /// Spawn a new instance of [`PlayerClose`] with the given `context`.
    pub fn spawn(context: PlayerCloseContext) -> Self {
        Self {
            handle: tokio::spawn(Self::task(context)),
        }
    }

    /// Get the [`JoinHandle`] for this instance.
    pub fn handle(&mut self) -> &mut JoinHandle<()> {
        &mut self.handle
    }

    /// Handle sending the given close code, and waiting for the echo from the
    /// client.
    #[tracing::instrument(name="player_close", skip_all)]
    async fn task(mut context: PlayerCloseContext) {
        let close_frame = Message::Close(Some(CloseFrame {
            code: context.close_code,
            reason: "".into(),
        }));

        // Display different information in the log depending on what was
        // provided.
        if let Some(handle_id) = context.maybe_handle_id {
            info!(%handle_id, close_code = %context.close_code, "sending");
        } else if let Some(player_id) = context.maybe_player_id {
            info!(%player_id, close_code = %context.close_code, "sending");
        } else {
            info!(close_code = %context.close_code, "sending");
        }

        match context.client_stream.send(close_frame).await {
            Ok(()) => {
                // Keep going through the stream until we find the echo frame,
                // or until time runs out.
                // TODO: Make sure this works as intended.
                let read_until_echo = Self::read_until_close_frame(
                        &mut context.client_stream, context.close_code);

                match timeout(WAIT_FOR_CLOSE_ECHO_DURATION, read_until_echo).await {
                    Ok(close_code_res) => match close_code_res {
                        Ok(close_code_received) => {
                            if close_code_received == context.close_code {
                                trace!("echo was expected value");
                            } else {
                                debug!(%close_code_received, "echo was not expected value");
                            }
                        },
                        Err(_) => {} // Call should output error log.
                    },
                    Err(_) => {
                        trace!("did not receive echo in time");
                    }
                }
            },
            Err(e) => {
                warn!(error = %e, "failed to send close code");
            },
        }

        if let Some(handle_id) = context.maybe_handle_id {
            info!(%handle_id, "connection closed");
        } else if let Some(player_id) = context.maybe_player_id {
            info!(%player_id, "connection closed");
        } else {
            info!("connection closed");
        }
    }

    /// Keep reading messages from the given client until one is a close frame,
    /// or until the stream is closed. Returns the close code given, or an error
    /// if the message is invalid, or the stream ended unexpectedly.
    /// TODO: Check if this function still counts under the player_close trace?
    async fn read_until_close_frame(stream: &mut PlayerStream, close_code_exp: CloseCode) -> Result<CloseCode, ()> {
        loop {
            match stream.next().await {
                Some(res) => match res {
                    Ok(msg) => match msg {
                        Message::Close(maybe_close_frame) => {
                            if let Some(close_frame) = maybe_close_frame {
                                return Ok(close_frame.code);
                            } else {
                                trace!("echo did not contain close frame");
                                return Err(());
                            }
                        },

                        // If the message is not a close frame, then we need
                        // to keep reading until we get one.
                        _ => {}
                    },
                    Err(e) => {
                        // TODO: Should this be a trace/debug instead?
                        warn!(error = %e, "error receiving echo event");
                        return Err(());
                    },
                }
                None => {
                    trace!("client closed connection early");
                    return Err(());
                }
            }
        }
    }
}

/*

/// The types of errors that can occur when parsing player messages.
#[derive(Debug)]
pub enum ParseError {
    NoNewline,
    NoSpace,
    UnexpectedPayload,
    NotInRoom,
    InvalidCommand,
    InvalidArgument,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoNewline => write!(f, "no new line character in message"),
            Self::NoSpace => write!(f, "no space character between command and argument"),
            Self::UnexpectedPayload => write!(f, "received a payload when we did not expect one"),
            Self::NotInRoom => write!(f, "cannot execute command when not in a room"),
            Self::InvalidCommand => write!(f, "invalid command"),
            Self::InvalidArgument => write!(f, "invalid argument"),
        }
    }
}

/// The data required to spawn a player instance.
#[derive(Debug)]
pub struct PlayerContext {
    /// The handle ID associated with this player.
    ///
    /// **NOTE:** This is NOT the same as their player ID, which is only
    /// assigned once they join a room.
    pub handle_id: HandleID,

    /// The WebSocket stream for this player.
    pub client_stream: PlayerStream,

    /// The channel for sending requests to the lobby.
    //pub lobby_sender: mpsc::Sender<LobbyRequest>,

    /// The channel for receiving configuration updates.
    pub config_receiver: watch::Receiver<VariableConfig>,

    /// The channel for receiving the main thread's shutdown signal.
    pub shutdown_signal: broadcast::Receiver<()>,
}

/// An instance which handles incoming player messages.
#[derive(Debug)]
pub struct Player {
    handle: JoinHandle<()>,
    remote_addr: SocketAddr,
}

// TODO: Split into two, one for players waiting to join a room, and one for
// players currently in a room? Think about the timelines for these handles,
// e.g. which bit should handle close codes? Ideally, everything related to the
// player should be in one place - but we may need to track certain handles
// in outside classes, like when we are waiting for a close code.

impl Player {
    /// Spawn a new player task with the given `context`.
    ///
    /// Although the task does not use it, the player's remote address is also
    /// required for tracking purposes.
    pub fn spawn(context: PlayerContext, remote_addr: SocketAddr) -> Self {
        Self {
            handle: tokio::spawn(Self::task(context)),
            remote_addr: remote_addr,
        }
    }

    /// Get the [`JoinHandle`] for this player's instance.
    pub fn handle(&mut self) -> &mut JoinHandle<()> {
        &mut self.handle
    }

    /// Get this player's remote address.
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    /// Handle the player's incoming messages.
    #[tracing::instrument(name = "player", skip_all, fields(handle_id = context.handle_id))]
    async fn task(mut context: PlayerContext) {
        // Get the server configuration as it is right now, and note what the
        // join time limit is.
        let current_config = *context.config_receiver.borrow_and_update();
        let join_time_limit_secs = current_config.join_room_time_limit_secs;
        let join_time_limit = Duration::from_secs(join_time_limit_secs);
        debug!(join_time_limit = %join_time_limit_secs, "read config");

        info!("waiting for request");
        match Self::wait_for_join_request(&mut context, join_time_limit).await {
            Ok(lobby_request) => {
                println!("lobby_request = {:?}", lobby_request);
            }
            Err(maybe_close_code) => {
                println!("maybe_close_code = {:?}", maybe_close_code);
            }
        }
    }

    /// Wait for the player's request to create or join a room in the lobby.
    ///
    /// If the player sends a valid request in time, it is returned.
    ///
    /// If the player sends an invalid request, or they do not send a request in
    /// time, then a close code might be returned. If one is returned, then we
    /// need to send it to the player and wait for their echo - otherwise, we
    /// can just drop the connection.
    #[tracing::instrument(name = "join", skip_all)]
    async fn wait_for_join_request(
        context: &mut PlayerContext,
        timeout_duration: Duration,
    ) -> Result<LobbyRequest, Option<CloseCode>> {
        tokio::select! {
            // TODO: The amount of indenting here is painful - find a nice way
            // to tidy it up!
            res = timeout(timeout_duration, context.client_stream.next()) => {
                if let Ok(maybe_frame) = res {
                    if let Some(maybe_frame) = maybe_frame {
                        match maybe_frame {
                            Ok(msg) => {
                                match msg {
                                    Message::Text(text) => {
                                        match Self::parse_lobby_message(&text) {
                                            Ok(request) => {
                                                info!(%request, "received");
                                                Ok(LobbyRequest {
                                                    handle_id: context.handle_id,
                                                    command: request,
                                                })
                                            },
                                            Err(e) => {
                                                warn!(error = %e, "received invalid message");
                                                match e {
                                                    // TODO: Make close codes
                                                    // more specific?
                                                    ParseError::NoNewline => Err(Some(PlayerCloseCode::InvalidFormat.into())),
                                                    ParseError::NoSpace => Err(Some(PlayerCloseCode::InvalidFormat.into())),
                                                    ParseError::UnexpectedPayload => Err(Some(PlayerCloseCode::InvalidFormat.into())),
                                                    ParseError::NotInRoom => Err(Some(PlayerCloseCode::NotInRoom.into())),
                                                    ParseError::InvalidCommand => Err(Some(PlayerCloseCode::InvalidCommand.into())),
                                                    ParseError::InvalidArgument => Err(Some(PlayerCloseCode::InvalidFormat.into())),
                                                }
                                            }
                                        }
                                    },

                                    Message::Binary(_) => {
                                        warn!("received binary message");
                                        Err(Some(CloseCode::Unsupported))
                                    },

                                    // We are not sending any pings during this
                                    // time, and as such, we do not expect any
                                    // pongs.
                                    Message::Ping(_) => {
                                        warn!("received ping");
                                        Err(Some(CloseCode::Protocol))
                                    },
                                    Message::Pong(_) => {
                                        warn!("received pong before client joined room");
                                        Err(Some(CloseCode::Protocol))
                                    },

                                    Message::Close(maybe_close_frame) => {
                                        if let Some(close_frame) = maybe_close_frame {
                                            info!(%close_frame, "received close message");
                                        } else {
                                            info!("received close message");
                                        }

                                        // TODO: Wait for ConnectionClosed message.
                                        Err(None)
                                    },

                                    // Should never get this while reading.
                                    Message::Frame(_) => {
                                        error!("received raw frame from stream");
                                        Err(None)
                                    },
                                }
                            },
                            Err(e) => {
                                warn!(error = %e, "websocket error in request");
                                match e {
                                    WebSocketError::ConnectionClosed => Err(None),
                                    WebSocketError::AlreadyClosed => Err(None),
                                    WebSocketError::Io(_) => Err(None),
                                    WebSocketError::Tls(_) => Err(Some(CloseCode::Protocol)),
                                    WebSocketError::Capacity(_) => Err(Some(CloseCode::Size)),
                                    WebSocketError::Protocol(_) => Err(Some(CloseCode::Protocol)),
                                    WebSocketError::WriteBufferFull(_) => Err(None),
                                    WebSocketError::Utf8 => Err(Some(CloseCode::Invalid)),
                                    WebSocketError::AttackAttempt => Err(None),
                                    WebSocketError::Url(_) => Err(None),
                                    WebSocketError::Http(_) => Err(Some(CloseCode::Protocol)),
                                    WebSocketError::HttpFormat(_) => Err(Some(CloseCode::Invalid)),
                                }
                            }
                        }
                    } else {
                        warn!("stream ended early");
                        Err(None)
                    }
                } else {
                    warn!("join request timeout");
                    Err(Some(PlayerCloseCode::DidNotJoinRoom.into()))
                }
            }
        }
    }

    /// Parse the player's message for creating or joining a room.
    fn parse_lobby_message(msg: &str) -> Result<LobbyCommand, ParseError> {
        if let Some((first_line, rest)) = msg.split_once('\n') {
            if !rest.is_empty() {
                return Err(ParseError::UnexpectedPayload);
            }

            if let Some((command, argument)) = first_line.split_once(' ') {
                match command {
                    "J:" => {
                        if argument.is_empty() {
                            Ok(LobbyCommand::CreateRoom)
                        } else if argument.len() == 4 {
                            if let Ok(room_code) = argument.try_into() {
                                Ok(LobbyCommand::JoinRoom(room_code))
                            } else {
                                Err(ParseError::InvalidArgument)
                            }
                        } else {
                            Err(ParseError::InvalidArgument)
                        }
                    }
                    "S:" => Err(ParseError::NotInRoom),
                    "O:" => Err(ParseError::NotInRoom),
                    "A:" => Err(ParseError::NotInRoom),
                    "C:" => Err(ParseError::NotInRoom),
                    _ => Err(ParseError::InvalidCommand),
                }
            } else {
                Err(ParseError::NoSpace)
            }
        } else {
            Err(ParseError::NoNewline)
        }
    }
}

*/
