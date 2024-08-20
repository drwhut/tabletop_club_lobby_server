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
#[derive(Debug, Clone, Copy)]
pub enum ClientUniqueID {
    /// The [`HandleID`] of the player if they are waiting to join a room.
    IsJoining(HandleID),

    /// The [`RoomCode`] and [`PlayerID`] of the player if they are in a room.
    HasJoined{ room_code: RoomCode, player_id: PlayerID },
}

impl fmt::Display for ClientUniqueID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

    /// The client's ID for logging purposes, if the client has one.
    pub client_id: Option<ClientUniqueID>,
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
#[tracing::instrument(name="send_close", skip_all, fields(client_id))]
pub async fn send_close(mut context: SendCloseContext) {
    // Since the `client_id` is within an `Option`, we need to set the log's
    // span field separately.
    let client_id = match context.client_id {
        Some(unique_id) => unique_id.to_string(),
        None => "none".to_string()
    };
    tracing::Span::current().record("client_id", &client_id);

    let close_frame = Message::Close(Some(CloseFrame {
        code: context.close_code,
        reason: "".into(),
    }));

    info!(close_code = %context.close_code, "sending");

    match context.client_stream.send(close_frame).await {
        Ok(()) => {
            // Keep going through the stream until we find the echo frame,
            // or until time runs out.
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
                "O:" => {
                    if let Ok(player_id) = argument.parse::<PlayerID>() {
                        // TODO: Add checks for the payload?
                        Ok(PlayerRequest::Offer(player_id, String::from(payload)))
                    } else {
                        Err(ParseError::InvalidArgument)
                    }
                },
                "A:" => {
                    if let Ok(player_id) = argument.parse::<PlayerID>() {
                        Ok(PlayerRequest::Answer(player_id, String::from(payload)))
                    } else {
                        Err(ParseError::InvalidArgument)
                    }
                },
                "C:" => {
                    if let Ok(player_id) = argument.parse::<PlayerID>() {
                        Ok(PlayerRequest::Candidate(player_id, String::from(payload)))
                    } else {
                        Err(ParseError::InvalidArgument)
                    }
                },
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn send_close_server(stream: PlayerStream, index: u16) -> Result<(), ()> {
        let close_code = CloseCode::Library(4000 + index);

        super::send_close(SendCloseContext {
            client_stream: stream,
            close_code,
            client_id: None
        }).await;

        Ok(())
    }

    #[tokio::test]
    async fn send_close() {
        let handle = crate::server_setup!(10000, 1, send_close_server);

        let mut stream = crate::client_setup!(10000);

        // TODO: Also test sending garbage data, sending an echo that's both
        // valid and invalid, once it becomes possible to send messages after
        // a close code has been received using tokio_tungstenite.
        // See: https://github.com/snapview/tokio-tungstenite/issues/310

        // Receiving a close code from the server, but not sending an echo back.
        assert_eq!(stream.next().await.unwrap().unwrap(),
                Message::Close(Some(CloseFrame {
                    code: CustomCloseCode::Error.into(),
                    reason: "".into()
                })));
        
        handle.await.expect("server was aborted");
    }

    async fn player_joining_server(stream: PlayerStream, index: u16) -> Result<(), ()> {
        let (request_send, mut request_receive) = tokio::sync::mpsc::channel(1);

        let mut task = PlayerJoining::spawn(PlayerJoiningContext {
            handle_id: 1,
            client_stream: stream,
            lobby_request_sender: request_send,
            max_wait_time: Duration::from_secs(5),
        });

        let request = request_receive.recv().await
                .expect("failed to receive request").0;

        if request.handle_id != 1 {
            return Err(());
        }

        let expected_command = match index {
            0 => LobbyCommand::CloseConnection(CustomCloseCode::DidNotJoinRoom.into()),
            1 => LobbyCommand::CloseConnection(CloseCode::Protocol),
            2 => LobbyCommand::CloseConnection(CloseCode::Invalid),
            3 => LobbyCommand::DropConnection,
            4 => LobbyCommand::CloseConnection(CloseCode::Protocol),
            5 => LobbyCommand::CloseConnection(CloseCode::Protocol),
            6 => LobbyCommand::CloseConnection(CloseCode::Unsupported),
            7 => LobbyCommand::CloseConnection(CustomCloseCode::InvalidFormat.into()),
            8 => LobbyCommand::CloseConnection(CustomCloseCode::InvalidFormat.into()),
            9 => LobbyCommand::CloseConnection(CustomCloseCode::InvalidFormat.into()),
            10 => LobbyCommand::CloseConnection(CustomCloseCode::InvalidCommand.into()),
            11 => LobbyCommand::CloseConnection(CustomCloseCode::InvalidDestination.into()),
            12 => LobbyCommand::CloseConnection(CustomCloseCode::InvalidDestination.into()),
            13 => LobbyCommand::CloseConnection(CustomCloseCode::NotInRoom.into()),
            14 => LobbyCommand::CloseConnection(CustomCloseCode::NotInRoom.into()),
            15 => LobbyCommand::CloseConnection(CustomCloseCode::NotInRoom.into()),
            16 => LobbyCommand::CloseConnection(CustomCloseCode::NotInRoom.into()),
            17 => LobbyCommand::CreateRoom,
            18 => LobbyCommand::JoinRoom("GGEZ".try_into().unwrap()),
            _ => LobbyCommand::DropConnection
        };

        if request.command != expected_command {
            return Err(());
        }

        task.handle().await.expect("task was aborted");

        Ok(())
    }

    #[tokio::test]
    async fn player_joining() {
        let handle = crate::server_setup!(10001, 19, player_joining_server);

        // Don't send a request - server should time us out.
        let mut stream = crate::client_setup!(10001);
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Drop the connection as soon as we can.
        {
            let _ = crate::client_setup!(10001);
        }

        // Send an invalid UTF-8 string.
        let mut stream = crate::client_setup!(10001);
        let invalid_utf8 = unsafe { String::from_utf8_unchecked(vec![103, 231, 46, 254]) };
        stream.send(Message::Text(invalid_utf8)).await.expect("failed to send");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Send a close code.
        let mut stream = crate::client_setup!(10001);
        stream.close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "".into()
        })).await.expect("failed to close stream");
        // TODO: Check if server echoed the close code, see above for why we
        // can't check this yet.

        // Send a ping.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Ping(vec![])).await.expect("failed to send ping");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Send a pong.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Pong(vec![])).await.expect("failed to send pong");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Send a binary message.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Binary(vec![0, 1])).await.expect("failed to send binary");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Send a request without a newline.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("J: ".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Send a request without a space.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("J:\n".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Send a request with an unexpected payload.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("J: \nHey!".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Send a request with an invalid command.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("B: \n".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Send a request with a room code that is the wrong length.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("J: ABCDEF\n".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Send a request with a room code that has invalid characters.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("J: abcd\n".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Try sealing a room before we have joined one.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("S: \n".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Try sending messages to other players before joining a room.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("O: 1\noffer".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("A: 1\nanswer".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("C: 1\ncandidate".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Send a request to host a room.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("J: \n".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        // Send a request to join a room.
        let mut stream = crate::client_setup!(10001);
        stream.send(Message::Text("J: GGEZ\n".into())).await.expect("failed to send message");
        stream.next().await.unwrap().expect_err("expected connection drop");

        handle.await.expect("server was aborted");
    }
}
