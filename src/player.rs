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
