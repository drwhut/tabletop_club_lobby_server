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

use crate::message::LobbyRequest;

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

    #[tracing::instrument]
    async fn task(mut context: Context) {
        let timeout_duration = *context.timeout_watch.borrow_and_update();

        let should_exit: Option<bool> = tokio::select! {
            res = timeout(timeout_duration, context.client_stream.next()) => {
                if let Ok(stream_data) = res {
                    println!("{:?}", stream_data);
                    None
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

    pub fn handle(&mut self) -> &mut JoinHandle<()> {
        &mut self.handle
    }
}
