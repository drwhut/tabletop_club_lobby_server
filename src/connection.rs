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

use crate::player::PlayerStream;

use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_native_tls::TlsAcceptor as TlsAcceptorAsync;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{accept_async_with_config, MaybeTlsStream};
use tracing::{debug, error, info, trace, warn};

/// The data required to accept WebSocket connections to the server.
pub struct ConnectionContext {
    /// The raw TCP stream for the connection.
    pub tcp_stream: TcpStream,

    /// An acceptor for encrypted TLS streams. If not given, the connection will
    /// not be encrypted.
    pub tls_acceptor: Option<TlsAcceptorAsync>,

    /// The client's address.
    ///
    /// **NOTE:** This should NOT be output anywhere, even for debug events!
    pub remote_addr: SocketAddr,

    /// The maximum size of incoming messages, in bytes.
    pub max_message_size: usize,

    /// The maximum size of incoming message payloads, in bytes.
    pub max_payload_size: usize,

    /// Send established connections and their remote addresses to the lobby.
    pub send_to_lobby: mpsc::Sender<(PlayerStream, SocketAddr)>,

    /// The shutdown signal receiver from the main thread.
    pub shutdown_signal: broadcast::Receiver<()>,
}

/// Attempt to accept the given connection.
#[tracing::instrument(name = "accept", skip_all)]
pub async fn accept_connection(mut context: ConnectionContext) {
    trace!("accepting connection");

    // The buffer size is dependent on the maximum message size, so we need to
    // make sure the maximum size isn't too small.
    if context.max_message_size < 100 {
        context.max_message_size = 100;
        warn!("max message size too low, set to {}", context.max_message_size);
    }

    // Since we could be asked to shutdown at any point in this process, use
    // the 'None' in Option to signal that we shouldn't continue.
    let maybe_stream = if let Some(acceptor) = context.tls_acceptor {
        tokio::select! {
            res = acceptor.accept(context.tcp_stream) => {
                match res {
                    Ok(tls_stream) => {
                        debug!("using encrypted tls stream");
                        Some(MaybeTlsStream::NativeTls(tls_stream))
                    },
                    Err(e) => {
                        error!(error = %e, "failed to accept tls connection");
                        None
                    }
                }
            },
            _ = context.shutdown_signal.recv() => None
        }
    } else {
        debug!("using unencrypted tcp stream");
        Some(MaybeTlsStream::Plain(context.tcp_stream))
    };

    if let Some(stream) = maybe_stream {
        // The 'max_send_queue' property is deprecated, but we need to assign it
        // in order to create the structure. See:
        // https://github.com/rust-lang/rust/issues/102777
        #[allow(deprecated)]
        let ws_config = WebSocketConfig {
            // Deprecated, but can't ignore it... >:(
            max_send_queue: None,

            // How much data needs to be written to the buffer before it is sent
            // to the stream - have it be a fraction of the maximum possible
            // message size, so that we're not over-allocating memory.
            write_buffer_size: context.max_message_size / 100,

            // The maximum size of the write buffer. The buffer will only build
            // up if writes are failing.
            max_write_buffer_size: context.max_message_size,

            // The maximum size of incoming messages.
            max_message_size: Some(context.max_message_size),

            // The maximum size of incoming message payloads.
            max_frame_size: Some(context.max_payload_size),

            // Do not allow unmasked frames, which are against the standard.
            accept_unmasked_frames: false,
        };

        debug!(
            write_buffer_size = ws_config.write_buffer_size,
            max_write_buffer_size = ws_config.max_write_buffer_size,
            max_message_size = ws_config.max_message_size,
            max_frame_size = ws_config.max_frame_size,
            accept_unmasked_frames = ws_config.accept_unmasked_frames
        );

        trace!("accepting websocket connection");

        let maybe_ws_stream = tokio::select! {
            res = accept_async_with_config(stream, Some(ws_config)) => {
                match res {
                    Ok(ws_stream) => Some(ws_stream),
                    Err(e) => {
                        error!(error = %e, "failed to accept websocket connection");
                        None
                    }
                }
            },
            _ = context.shutdown_signal.recv() => None
        };

        if let Some(ws_stream) = maybe_ws_stream {
            info!("connection established");

            // Send the stream and its remote address to the lobby.
            let res = context
                .send_to_lobby
                .send((ws_stream, context.remote_addr))
                .await;
            if let Err(e) = res {
                warn!(error = %e, "lobby receiver dropped, assuming server is shutting down");
            }
        } else {
            trace!("skipped accepting websocket connection");
        }
    } else {
        trace!("skipped creating websocket connection");
    }

    trace!("finished accepting connection");
}
