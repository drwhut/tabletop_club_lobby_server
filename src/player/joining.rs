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

use super::*;

use crate::close_code::{CloseCode, CustomCloseCode};
use crate::message::{LobbyCommand, LobbyRequest};

use futures_util::stream::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, trace, warn};

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
                                    error!(request = %req, "client sent request before joining a room");
                                    LobbyCommand::CloseConnection(CustomCloseCode::NotInRoom.into())
                                }
                            },
                            Err(e) => {
                                error!(error = %e, "error parsing request from client");
                                LobbyCommand::CloseConnection(parse_error_to_close_code(e))
                            }
                        },
                        Message::Binary(_) => {
                            error!("received binary message from client");
                            LobbyCommand::CloseConnection(CloseCode::Unsupported)
                        }
                        Message::Ping(_) => {
                            error!("received ping from client");
                            LobbyCommand::CloseConnection(CloseCode::Protocol)
                        }
                        Message::Pong(_) => {
                            error!("received pong from client before joining a room");
                            LobbyCommand::CloseConnection(CloseCode::Protocol)
                        }
                        Message::Close(maybe_close_frame) => {
                            if let Some(close_frame) = maybe_close_frame {
                                if close_frame.code == CloseCode::Normal {
                                    info!("client is closing connection");
                                } else {
                                    warn!(code = %close_frame,
                                            "client sent close message");
                                }
                            } else {
                                warn!("client sent close message");
                            }

                            // We don't need to manually send a close frame
                            // back, as the library does this for us.
                            LobbyCommand::DropConnection
                        }
                        Message::Frame(_) => {
                            // Should never get this while reading.
                            error!("received raw frame from stream");
                            LobbyCommand::DropConnection
                        }
                    },
                    Err(e) => {
                        error!(error = %e, "error receiving request from client");
                        match websocket_error_to_close_code(e) {
                            Some(close_code) => LobbyCommand::CloseConnection(close_code),
                            None => LobbyCommand::DropConnection,
                        }
                    }
                },
                None => {
                    error!("client stream ended early");
                    LobbyCommand::DropConnection
                }
            },
            Err(_) => {
                error!("did not receive a request from the client in time");
                LobbyCommand::CloseConnection(CustomCloseCode::DidNotJoinRoom.into())
            }
        };

        let request = LobbyRequest {
            handle_id: context.handle_id,
            command,
        };
        match context
            .lobby_request_sender
            .send((request, context.client_stream))
            .await
        {
            Ok(_) => trace!("sent request to the lobby"),
            Err(e) => error!(error = %e, "lobby request receiver dropped"),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::broadcast;

    async fn player_joining_server(
        stream: PlayerStream,
        index: u16,
        _shutdown: broadcast::Receiver<()>,
    ) -> Result<(), ()> {
        let (request_send, mut request_receive) = tokio::sync::mpsc::channel(1);

        let mut task = PlayerJoining::spawn(PlayerJoiningContext {
            handle_id: 1,
            client_stream: stream,
            lobby_request_sender: request_send,
            max_wait_time: Duration::from_secs(5),
        });

        let request = request_receive
            .recv()
            .await
            .expect("failed to receive request")
            .0;

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
            _ => LobbyCommand::DropConnection,
        };

        if request.command != expected_command {
            return Err(());
        }

        task.handle().await.expect("task was aborted");

        Ok(())
    }

    #[tokio::test]
    async fn player_joining() {
        let (handle, _) = crate::server_setup!(10001, 19, player_joining_server);

        // Don't send a request - server should time us out.
        let mut stream = crate::client_setup!(10001);
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Drop the connection as soon as we can.
        {
            let _ = crate::client_setup!(10001);
        }

        // Send an invalid UTF-8 string.
        let mut stream = crate::client_setup!(10001);
        let invalid_utf8 = unsafe { String::from_utf8_unchecked(vec![103, 231, 46, 254]) };
        stream
            .send(Message::Text(invalid_utf8))
            .await
            .expect("failed to send");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Send a close code.
        let mut stream = crate::client_setup!(10001);
        stream
            .close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "".into(),
            }))
            .await
            .expect("failed to close stream");
        // TODO: Check if server echoed the close code, see above for why we
        // can't check this yet.

        // Send a ping.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Ping(vec![]))
            .await
            .expect("failed to send ping");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Send a pong.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Pong(vec![]))
            .await
            .expect("failed to send pong");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Send a binary message.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Binary(vec![0, 1]))
            .await
            .expect("failed to send binary");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Send a request without a newline.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("J: ".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Send a request without a space.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("J:\n".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Send a request with an unexpected payload.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("J: \nHey!".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Send a request with an invalid command.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("B: \n".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Send a request with a room code that is the wrong length.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("J: ABCDEF\n".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Send a request with a room code that has invalid characters.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("J: abcd\n".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Try sealing a room before we have joined one.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("S: \n".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Try sending messages to other players before joining a room.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("O: 1\noffer".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("A: 1\nanswer".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("C: 1\ncandidate".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Send a request to host a room.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("J: \n".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        // Send a request to join a room.
        let mut stream = crate::client_setup!(10001);
        stream
            .send(Message::Text("J: GGEZ\n".into()))
            .await
            .expect("failed to send message");
        stream
            .next()
            .await
            .unwrap()
            .expect_err("expected connection drop");

        handle.await.expect("server was aborted");
    }
}
