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

    /// Send established connections to the lobby.
    pub send_to_lobby: mpsc::Sender<PlayerStream>,

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
        warn!(
            "max message size too low, set to {}",
            context.max_message_size
        );
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

            // Send the stream to the lobby.
            let res = context.send_to_lobby.send(ws_stream).await;
            if let Err(e) = res {
                error!(error = %e, "lobby receiver dropped, is the server shutting down?");
            }
        } else {
            trace!("skipped accepting websocket connection");
        }
    } else {
        trace!("skipped creating websocket connection");
    }

    trace!("finished accepting connection");
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    struct TestContext {
        pub port: u16,
        pub tls_acceptor: Option<TlsAcceptorAsync>,
        pub max_payload_size: usize,
        pub send_shutdown: bool,
    }

    async fn test_with_context(test: TestContext) {
        let is_encrypted = test.tls_acceptor.is_some();

        let (ready_send, ready_receive) = tokio::sync::oneshot::channel();
        let (lobby_send, mut lobby_receive) = tokio::sync::mpsc::channel(1);
        let (shutdown_send, shutdown_receive) = tokio::sync::broadcast::channel(1);

        let handle = tokio::spawn(async move {
            let server_addr = format!("127.0.0.1:{}", test.port);
            let listener = tokio::net::TcpListener::bind(server_addr)
                .await
                .expect("failed to create listener");
            ready_send.send(()).expect("failed to send ready signal");

            let (conn, addr) = listener
                .accept()
                .await
                .expect("failed to accept connection");

            accept_connection(ConnectionContext {
                tcp_stream: conn,
                tls_acceptor: test.tls_acceptor,
                remote_addr: addr,
                max_message_size: test.max_payload_size + 100,
                max_payload_size: test.max_payload_size,
                send_to_lobby: lobby_send,
                shutdown_signal: shutdown_receive,
            })
            .await;
        });

        ready_receive.await.expect("server is not ready");

        let server_addr = format!("localhost:{}", test.port);
        let tcp = tokio::net::TcpStream::connect(server_addr.clone())
            .await
            .expect("failed to connect");

        let maybe_tls = if is_encrypted {
            // Add the test certificate as trusted.
            let cert_bytes = tokio::fs::read("tests/certificate.pem")
                .await
                .expect("failed to read test certificate");
            let cert = tokio_native_tls::native_tls::Certificate::from_pem(&cert_bytes)
                .expect("failed to load test certificate");

            let sync_connector = tokio_native_tls::native_tls::TlsConnector::builder()
                .add_root_certificate(cert)
                .build()
                .expect("failed to build tls connector");

            let async_connector: tokio_native_tls::TlsConnector = sync_connector.into();
            let tls_stream = async_connector
                .connect("localhost", tcp)
                .await
                .expect("failed to establish tls connection");

            tokio_tungstenite::MaybeTlsStream::NativeTls(tls_stream)
        } else {
            tokio_tungstenite::MaybeTlsStream::Plain(tcp)
        };

        let req_addr = if is_encrypted {
            format!("wss://{}", server_addr)
        } else {
            format!("ws://{}", server_addr)
        };

        if test.send_shutdown {
            shutdown_send
                .send(())
                .expect("failed to send shutdown signal");
            tokio_tungstenite::client_async(req_addr, maybe_tls)
                .await
                .expect_err("shutdown did not stop accept");
        } else {
            let (mut stream, _) = tokio_tungstenite::client_async(req_addr, maybe_tls)
                .await
                .expect("client failed to connect");

            // When the stream is accepted, it should be sent to the lobby along
            // with the client's address.
            let mut srv_stream = lobby_receive
                .recv()
                .await
                .expect("did not receive accepted stream");

            // Send a message within the payload limit.
            let message_ok = "A".repeat(test.max_payload_size);
            stream
                .send(Message::Text(message_ok.clone()))
                .await
                .expect("failed to send message");
            let recv = srv_stream
                .next()
                .await
                .unwrap()
                .expect("failed to receive valid message");
            assert_eq!(recv, Message::Text(message_ok));

            // Send a message outside of the payload limit.
            let message_err = "E".repeat(test.max_payload_size + 1);
            stream
                .send(Message::Text(message_err.clone()))
                .await
                .expect("failed to send message");
            srv_stream
                .next()
                .await
                .unwrap()
                .expect_err("invalid message was received as valid");
        }

        handle.await.expect("task was aborted");
    }

    #[tokio::test]
    async fn accept_plain() {
        test_with_context(TestContext {
            port: 11000,
            tls_acceptor: None,
            max_payload_size: 1000,
            send_shutdown: false,
        })
        .await;
    }

    #[tokio::test]
    async fn accept_tls() {
        let cert_bytes = tokio::fs::read("tests/certificate.pem")
            .await
            .expect("failed to read test certificate");
        let key_bytes = tokio::fs::read("tests/private.pem")
            .await
            .expect("failed to read test private key");

        let identity = tokio_native_tls::native_tls::Identity::from_pkcs8(&cert_bytes, &key_bytes)
            .expect("failed to create cryptographic identity");

        let sync_acceptor = tokio_native_tls::native_tls::TlsAcceptor::new(identity)
            .expect("failed to create acceptor from identity");
        let async_acceptor = tokio_native_tls::TlsAcceptor::from(sync_acceptor);

        test_with_context(TestContext {
            port: 11001,
            tls_acceptor: Some(async_acceptor),
            max_payload_size: 10000,
            send_shutdown: false,
        })
        .await;
    }

    #[tokio::test]
    async fn check_shutdown() {
        test_with_context(TestContext {
            port: 11002,
            tls_acceptor: None,
            max_payload_size: 100,
            send_shutdown: true,
        })
        .await;
    }
}
