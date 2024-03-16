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

use crate::config::VariableConfig;
use crate::message::{LobbyCommand, LobbyRequest};

use futures_util::sink::{Close, SinkExt};
use futures_util::stream::StreamExt;
use std::borrow::Cow;
use std::fmt;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
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

/// A custom set of close codes that the server can send to the players.
pub enum PlayerCloseCode {
    /// A generic error occured.
    ///
    /// **NOTE:** This close code should never be used. Always try to use a more
    /// descriptive close code.
    Error,

    /// Failed to connect to the lobby server.
    ///
    /// **NOTE:** This close code should never be used. This is meant as an
    /// error code for clients, as is here only for reference.
    Unreachable,

    /// The client did not create or join a room in time.
    DidNotJoinRoom,

    /// The host disconnected from the room the player was in.
    HostDisconnected,

    /// Only the host of the room is allowed to seal it.
    OnlyHostCanSeal,

    /// The maximum number of rooms has been reached.
    TooManyRooms,

    /// Tried to do something that can only be done when not in a room.
    AlreadyInRoom,

    /// The room that was given by the client does not exist.
    RoomDoesNotExist,

    /// The room that was given by the client has been sealed.
    RoomSealed,

    /// The message that was given was incorrectly formatted.
    InvalidFormat,

    /// Tried to do something that can only be done while in a room.
    NotInRoom,

    /// An internal server error occured.
    ///
    /// **NOTE:** This close code is redundant, [`CloseCode::Error`] should be
    /// used instead.
    ServerError,

    /// The message that was given contained an invalid destination ID.
    InvalidDestination,

    /// The message that was given contained an invalid command.
    InvalidCommand,

    /// The maximum number of players has been reached.
    ///
    /// **NOTE:** This close code is now redundant.
    TooManyPlayers,

    /// A binary message was received when only text messages are allowed.
    ///
    /// **NOTE:** This close code is redundant, [`CloseCode::Unsupported`]
    /// should be used instead.
    InvalidMode,

    /// Too many connections from the same IP address.
    TooManyConnections,

    /// Established another connection too quickly after the last one.
    ReconnectTooQuickly,

    /// The queue of players trying to create or join rooms is at capacity.
    JoinQueueFull,
}

impl From<PlayerCloseCode> for u16 {
    fn from(value: PlayerCloseCode) -> Self {
        match value {
            PlayerCloseCode::Error => 4000,
            PlayerCloseCode::Unreachable => 4001,
            PlayerCloseCode::DidNotJoinRoom => 4002,
            PlayerCloseCode::HostDisconnected => 4003,
            PlayerCloseCode::OnlyHostCanSeal => 4004,
            PlayerCloseCode::TooManyRooms => 4005,
            PlayerCloseCode::AlreadyInRoom => 4006,
            PlayerCloseCode::RoomDoesNotExist => 4007,
            PlayerCloseCode::RoomSealed => 4008,
            PlayerCloseCode::InvalidFormat => 4009,
            PlayerCloseCode::NotInRoom => 4010,
            PlayerCloseCode::ServerError => 4011,
            PlayerCloseCode::InvalidDestination => 4012,
            PlayerCloseCode::InvalidCommand => 4013,
            PlayerCloseCode::TooManyPlayers => 4014,
            PlayerCloseCode::InvalidMode => 4015,
            PlayerCloseCode::TooManyConnections => 4016,
            PlayerCloseCode::ReconnectTooQuickly => 4017,
            PlayerCloseCode::JoinQueueFull => 4018,
        }
    }
}

impl TryFrom<u16> for PlayerCloseCode {
    type Error = ();

    fn try_from(value: u16) -> Result<Self, <Self as TryFrom<u16>>::Error> {
        match value {
            4000 => Ok(Self::Error),
            4001 => Ok(Self::Unreachable),
            4002 => Ok(Self::DidNotJoinRoom),
            4003 => Ok(Self::HostDisconnected),
            4004 => Ok(Self::OnlyHostCanSeal),
            4005 => Ok(Self::TooManyRooms),
            4006 => Ok(Self::AlreadyInRoom),
            4007 => Ok(Self::RoomDoesNotExist),
            4008 => Ok(Self::RoomSealed),
            4009 => Ok(Self::InvalidFormat),
            4010 => Ok(Self::NotInRoom),
            4011 => Ok(Self::ServerError),
            4012 => Ok(Self::InvalidDestination),
            4013 => Ok(Self::InvalidCommand),
            4014 => Ok(Self::TooManyPlayers),
            4015 => Ok(Self::InvalidMode),
            4016 => Ok(Self::TooManyConnections),
            4017 => Ok(Self::ReconnectTooQuickly),
            4018 => Ok(Self::JoinQueueFull),

            _ => Err(()),
        }
    }
}

impl From<PlayerCloseCode> for CloseCode {
    fn from(value: PlayerCloseCode) -> Self {
        Self::Library(value.into())
    }
}

impl TryFrom<CloseCode> for PlayerCloseCode {
    type Error = ();

    fn try_from(value: CloseCode) -> Result<Self, <Self as TryFrom<CloseCode>>::Error> {
        if let CloseCode::Library(code_u16) = value {
            PlayerCloseCode::try_from(code_u16)
        } else {
            Err(())
        }
    }
}
