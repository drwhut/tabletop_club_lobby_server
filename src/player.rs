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

use crate::message::{LobbyCommand, LobbyRequest};

use futures_util::sink::{Close, SinkExt};
use futures_util::stream::StreamExt;
use std::borrow::Cow;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{error, info};

pub type HandleID = u32;
pub type PlayerID = u32;

pub type PlayerStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub enum ParseError {
    NoNewline,
    NoSpace,
    UnexpectedPayload,
    NotInLobby,
    InvalidCommand,
    InvalidArgument,
}

#[derive(Debug)]
pub struct Context {
    pub handle_id: HandleID,
    pub client_stream: PlayerStream,

    pub lobby_sender: mpsc::Sender<LobbyRequest>,
    pub timeout_watch: watch::Receiver<Duration>,
    pub shutdown_signal: broadcast::Receiver<()>,
}

#[derive(Debug)]
pub struct Player {
    handle: JoinHandle<()>,
}

impl Player {
    pub fn spawn(context: Context) -> Self {
        Self {
            handle: tokio::spawn(Self::task(context)),
        }
    }

    pub fn handle(&mut self) -> &mut JoinHandle<()> {
        &mut self.handle
    }

    #[tracing::instrument(level = "trace", skip(context))]
    async fn task(mut context: Context) {
        let timeout_duration = *context.timeout_watch.borrow_and_update();

        let should_exit: Option<bool> = tokio::select! {
            res = timeout(timeout_duration, context.client_stream.next()) => {
                if let Ok(stream_res) = res {
                    println!("{:?}", stream_res);

                    if let Some(message) = stream_res {
                        match message {
                            Ok(message) => {
                                match message {
                                    Message::Text(text) => {
                                        None
                                    },
                                    _ => None,
                                }
                            },
                            Err(e) => {
                                // Protocol error.
                                None
                            },
                        }
                    } else {
                        // No more items can be gotten from the stream.
                        println!("Stream closed!");
                        Some(false)
                    }
                } else {
                    let close_msg = Message::Close(Some(CloseFrame{
                        code: CloseCode::Library(4002),
                        reason: Cow::Borrowed("did not join in time"),
                    }));

                    let res = context.client_stream.send(close_msg).await;
                    Some(res.is_ok())
                }
            },
            res = context.shutdown_signal.recv() => {
                if let Err(e) = res {
                    // TODO: Error.
                }

                let close_msg = Message::Close(Some(CloseFrame{
                    code: CloseCode::Away,
                    reason: Cow::Borrowed(""),
                }));

                let res = context.client_stream.send(close_msg).await;
                Some(res.is_ok())
            }
        };

        println!("{:?}", should_exit);

        println!(
            "echo? {:?}",
            timeout(timeout_duration, context.client_stream.next()).await
        );
    }

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
                    "S:" => Err(ParseError::NotInLobby),
                    "O:" => Err(ParseError::NotInLobby),
                    "A:" => Err(ParseError::NotInLobby),
                    "C:" => Err(ParseError::NotInLobby),
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
