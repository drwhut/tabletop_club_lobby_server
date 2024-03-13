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

use crate::player::{Context, Player};

use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, MaybeTlsStream};

pub async fn listen() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:9080").await?;

    match listener.accept().await {
        Ok((socket, addr)) => {
            let socket = MaybeTlsStream::Plain(socket);
            let ws_stream = accept_async(socket).await.expect("failed to accept ws");

            let (lobby_sender, lobby_receiver) = tokio::sync::mpsc::channel(10);
            let (timeout_sender, timeout_receiver) =
                tokio::sync::watch::channel(tokio::time::Duration::from_secs(10));
            let (shutdown_sender, shutdown_receiver) = tokio::sync::broadcast::channel(1);

            let context = Context {
                handle_id: 1,
                client_stream: ws_stream,
                lobby_sender: lobby_sender,
                timeout_watch: timeout_receiver,
                shutdown_signal: shutdown_receiver,
            };

            let mut player = Player::spawn(context);
            player.handle().await.expect("player handle errored");
        }
        Err(e) => {
            println!("error: {:?}", e);
        }
    };

    Ok(())
}
