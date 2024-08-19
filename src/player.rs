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

use crate::close_code::{CloseCode, CustomCloseCode};
use crate::message::{LobbyCommand, LobbyRequest};
use crate::room_code::RoomCode;

use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use std::fmt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, trace, warn};

/// The type used for handle IDs, which are for players that have yet to join a
/// room.
pub type HandleID = u32;

/// The type used for player IDs, which are for players that are currently in a
/// room. This is the ID clients use to communicate with each other.
pub type PlayerID = u32;

/// A type alias for a player's WebSocket stream.
pub type PlayerStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The maximum amount of time to wait for the client to send back an echoed
/// close frame after we have sent one.
const WAIT_FOR_CLOSE_ECHO_DURATION: Duration = Duration::from_secs(5);

/// Data which uniquely identifies a client within the server.
#[derive(Debug)]
pub enum ClientUniqueID {
    /// The player will not have an ID only if they have yet to be in the join
    /// queue.
    /// TODO: Should clients always be given a [`HandleID`], even if they don't
    /// end up joining the queue?
    None,

    /// The [`HandleID`] of the player if they are waiting to join a room.
    IsJoining(HandleID),

    /// The [`RoomCode`] and [`PlayerID`] of the player if they are in a room.
    HasJoined{ room_code: RoomCode, player_id: PlayerID },
}

impl fmt::Display for ClientUniqueID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            // TODO: See how this format looks in the logs.
            Self::IsJoining(handle_id) => write!(f, "handle_id={}", handle_id),
            Self::HasJoined{ room_code, player_id } => write!(f, "room_code={} player_id={}", room_code, player_id),
        }
    }
}

/// The data required to spawn the [`send_close`] task.
#[derive(Debug)]
pub struct SendCloseContext {
    /// The WebSocket stream for this player.
    pub client_stream: PlayerStream,

    /// The [`CloseCode`] to send to, and expect back from, the client.
    pub close_code: CloseCode,

    /// The client's ID for logging purposes.
    pub client_id: ClientUniqueID,
}

/// A task which sends a close code to a given player, and then waits for the
/// client to echo it back, up to a given amount of time.
/// 
/// This task should be used whenever the server wants to initialise the process
/// of closing the connection with a client. If however the client is the one to
/// initialise it, then the instance handling the client connection should
/// simply echo it back, send messages where necessary, and then drop the
/// connection out of scope.
/// 
/// Since this task can potentially take up to a few seconds to complete, it
/// should be added to a task tracker so that connections are closed gracefully
/// when the server is shutting down.
#[tracing::instrument(name="send_close", skip_all, fields(client_id = %context.client_id))]
pub async fn send_close(mut context: SendCloseContext) {
    let close_frame = Message::Close(Some(CloseFrame {
        code: context.close_code,
        reason: "".into(),
    }));
    
    info!(close_code = %context.close_code, "sending");

    match context.client_stream.send(close_frame).await {
        Ok(()) => {
            // Keep going through the stream until we find the echo frame,
            // or until time runs out.
            // TODO: Make sure this works as intended.
            let read_until_echo = read_until_close_frame(&mut context.client_stream);

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

    info!("connection closed");
}

/// Keep reading messages from the given client until one is a close frame,
/// or until the stream is closed. Returns the close code given, or an error
/// if the message is invalid, or the stream ended unexpectedly.
/// TODO: Check if this function still counts under the player_close trace?
async fn read_until_close_frame(stream: &mut PlayerStream) -> Result<CloseCode, ()> {
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

/// The data required to spawn a [`PlayerJoining`] instance.
pub struct PlayerJoiningContext {
    /// The [`HandleID`] associated with this player.
    pub handle_id: HandleID,

    /// The WebSocket stream for this player.
    pub client_stream: PlayerStream,

    /// A channel for sending [`LobbyRequest`] once we have received a message
    /// from the client.
    pub lobby_request_sender: mpsc::Sender<(LobbyRequest, PlayerStream)>,

    /// The maximum amount of time the instance will wait for the client to send
    /// a request.
    pub max_wait_time: Duration,
}

/// An instance which, just after the client has established a connection to the
/// server, waits for the client's request to either host a new room, or join an
/// existing room.
/// 
/// The instance will send a [`LobbyRequest`] to the [`Lobby`] once it is done
/// executing. This will either contain the request provided by the client, or
/// a request to either send a close code to the client using [`PlayerClose`],
/// or to simply drop the connection (if for example, the client sent a close
/// code, and we echoed it straight back).
pub struct PlayerJoining {
    handle: JoinHandle<()>,
}

impl PlayerJoining {
    /// Spawn a new instance of [`PlayerJoining`] with the given `context`.
    pub fn spawn(context: PlayerJoiningContext) -> Self {
        Self {
            handle: tokio::spawn(Self::task(context)),
        }
    }

    /// Get the [`JoinHandle`] for this instance.
    pub fn handle(&mut self) -> &mut JoinHandle<()> {
        &mut self.handle
    }

    /// Handle waiting for the client's initial host or join request.
    #[tracing::instrument(name="player_joining", skip_all, fields(handle_id = context.handle_id))]
    async fn task(mut context: PlayerJoiningContext) {
        trace!("waiting for initial request");

        let command = match timeout(context.max_wait_time, context.client_stream.next()).await {
            Ok(res) => match res {
                Some(res) => match res {
                    Ok(msg) => match msg {
                        Message::Text(text) => match parse_player_request(&text) {
                            Ok(req) => match req {
                                PlayerRequest::Host => LobbyCommand::CreateRoom,
                                PlayerRequest::Join(room_code) => LobbyCommand::JoinRoom(room_code),
                                _ => {
                                    warn!(request = %req, "client sent request while not in a room");
                                    LobbyCommand::CloseConnection(CustomCloseCode::NotInRoom.into())
                                }
                            },
                            Err(e) => {
                                warn!(error = %e, "error parsing request");
                                LobbyCommand::CloseConnection(parse_error_to_close_code(e))
                            },
                        },
                        Message::Binary(_) => {
                            warn!("received binary message");
                            LobbyCommand::CloseConnection(CloseCode::Unsupported)
                        },
                        Message::Ping(_) => {
                            warn!("received ping");
                            LobbyCommand::CloseConnection(CloseCode::Protocol)
                        },
                        Message::Pong(_) => {
                            warn!("received pong before client joined room");
                            LobbyCommand::CloseConnection(CloseCode::Protocol)
                        },
                        Message::Close(maybe_close_frame) => {
                            let echo_frame = match maybe_close_frame {
                                Some(close_frame) => CloseFrame {
                                    code: close_frame.code,
                                    reason: close_frame.reason
                                },
                                None => CloseFrame {
                                    code: CloseCode::Status,
                                    reason: "".into()
                                },
                            };

                            debug!(close_code = %echo_frame.code,
                                    "client sent close message");

                            // As per the protocol, send back an echo of the
                            // close message.
                            let echo = Message::Close(Some(echo_frame));
                            if let Err(e) = context.client_stream.send(echo).await {
                                error!(error = %e, "error sending close echo");
                            };

                            LobbyCommand::DropConnection
                        },
                        Message::Frame(_) => {
                            // Should never get this while reading.
                            error!("received raw frame from stream");
                            LobbyCommand::DropConnection
                        },
                    },
                    Err(e) => {
                        warn!(error = %e, "websocket error");
                        match websocket_error_to_close_code(e) {
                            Some(close_code) => LobbyCommand::CloseConnection(close_code),
                            None => LobbyCommand::DropConnection,
                        }
                    },
                },
                None => {
                    // TODO: Re-think which level these kinds of logs should be.
                    // Remember that the trace level has a ton of debugging info
                    // from the WebSocket library.
                    trace!("stream ended");
                    LobbyCommand::DropConnection
                },
            },
            Err(_) => {
                trace!("client did not send a request in time");
                LobbyCommand::CloseConnection(CustomCloseCode::DidNotJoinRoom.into())
            },
        };

        let request = LobbyRequest { handle_id: context.handle_id, command };
        match context.lobby_request_sender.send((request, context.client_stream)).await {
            Ok(_) => trace!("sent request to the lobby"),
            Err(_) => error!("lobby request receiver dropped"),
        };
    }
}

/// The requests that players can make to the server.
enum PlayerRequest {
    Host,
    Join(RoomCode),
    Seal,
    Offer(PlayerID, String),
    Answer(PlayerID, String),
    Candidate(PlayerID, String),
}

impl fmt::Display for PlayerRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host => write!(f, "host"),
            Self::Join(room_code) => write!(f, "join {}", room_code),
            Self::Seal => write!(f, "seal"),

            // Do not display payloads, as they contain sensitive info.
            Self::Offer(player_id, _) => write!(f, "offer {}", player_id),
            Self::Answer(player_id, _) => write!(f, "answer {}", player_id),
            Self::Candidate(player_id, _) => write!(f, "candidate {}", player_id),
        }
    }
}

/// The types of errors that can occur when parsing player messages.
#[derive(Debug)]
enum ParseError {
    NoNewline,
    NoSpace,
    UnexpectedPayload,
    InvalidCommand,
    InvalidArgument,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoNewline => write!(f, "no new line character in message"),
            Self::NoSpace => write!(f, "no space character between command and argument"),
            Self::UnexpectedPayload => write!(f, "received a payload when we did not expect one"),
            Self::InvalidCommand => write!(f, "invalid command"),
            Self::InvalidArgument => write!(f, "invalid argument"),
        }
    }
}

/// Parse the text message given by a player into a [`PlayerRequest`].
fn parse_player_request(msg: &str) -> Result<PlayerRequest, ParseError> {
    if let Some((first_line, payload)) = msg.split_once('\n') {
        if let Some((command, argument)) = first_line.split_once(' ') {
            match command {
                "J:" => {
                    if !payload.is_empty() {
                        return Err(ParseError::UnexpectedPayload);
                    }

                    if argument.is_empty() {
                        Ok(PlayerRequest::Host)
                    } else if argument.len() == 4 {
                        if let Ok(room_code) = argument.try_into() {
                            Ok(PlayerRequest::Join(room_code))
                        } else {
                            Err(ParseError::InvalidArgument)
                        }
                    } else {
                        Err(ParseError::InvalidArgument)
                    }
                },
                "S:" => {
                    if !payload.is_empty() {
                        return Err(ParseError::UnexpectedPayload);
                    }

                    if !argument.is_empty() {
                        return Err(ParseError::InvalidArgument);
                    }

                    Ok(PlayerRequest::Seal)
                },
                "O:" => todo!(), // TODO: Add checks for payload.
                "A:" => todo!(), // Does library check for valid UTF-8?
                "C:" => todo!(),
                _ => Err(ParseError::InvalidCommand),
            }
        } else {
            Err(ParseError::NoSpace)
        }
    } else {
        Err(ParseError::NoNewline)
    }
}

/// Convert a [`ParseError`] into a [`CloseCode`] that can be sent back to the
/// client, so that they knoe what went wrong.
fn parse_error_to_close_code(e: ParseError) -> CloseCode {
    match e {
        ParseError::NoNewline => CustomCloseCode::InvalidFormat.into(),
        ParseError::NoSpace => CustomCloseCode::InvalidFormat.into(),
        ParseError::UnexpectedPayload => CustomCloseCode::InvalidFormat.into(),
        ParseError::InvalidCommand => CustomCloseCode::InvalidCommand.into(),
        ParseError::InvalidArgument => CustomCloseCode::InvalidDestination.into(),
    }
}

/// Potentially convert a WebSocket error into a [`CloseCode`] that can be sent
/// back to the client, so that they know what went wrong.
/// 
/// If `None` is returned, then the connection should just be dropped.
fn websocket_error_to_close_code(e: WebSocketError) -> Option<CloseCode> {
    match e {
        WebSocketError::ConnectionClosed => None,
        WebSocketError::AlreadyClosed => None,
        WebSocketError::Io(_) => None, // Error in connection is fatal.
        WebSocketError::Tls(_) => Some(CloseCode::Protocol),
        WebSocketError::Capacity(_) => Some(CloseCode::Size),
        WebSocketError::Protocol(_) => Some(CloseCode::Protocol),
        WebSocketError::WriteBufferFull(_) => None, // Can't send close message.
        WebSocketError::Utf8 => Some(CloseCode::Invalid),
        WebSocketError::AttackAttempt => None, // If I can't see it, it's not there...
        WebSocketError::Url(_) => None, // Can't send any message.
        WebSocketError::Http(_) => Some(CloseCode::Protocol),
        WebSocketError::HttpFormat(_) => Some(CloseCode::Invalid),
    }
}
