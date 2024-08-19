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

//! Helper macros used for unit tests throughout the server.

/// Creates the following types for use by the other macros:
/// 
/// - `WSStream`: WebSocket stream.
#[macro_export]
macro_rules! server_types {
    () => {
        type WSStream = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;
    }
}

/// Set up the server task with the following parameters:
/// 
/// - The port number to bind to. **NOTE:** Make sure to use a different port
///   for each test, otherwise some tests may fail!
/// - The number of connections to accept before the task stops.
/// - The function to call after each connection is accepted.
/// 
/// The handle for the spawned task is returned.
#[macro_export]
macro_rules! server_setup {
    ($p:literal, $n:literal, $f:ident) => {{
        let (ready_send, ready_receive) = tokio::sync::oneshot::channel();

        let handle = tokio::spawn(async {
            let server_addr = format!("127.0.0.1:{}", $p);
            let listener = tokio::net::TcpListener::bind(server_addr).await
                    .expect("failed to create listener");
            ready_send.send(()).expect("failed to send ready signal");

            for client_index in 0..$n {
                let (connection, _) = listener.accept().await
                        .expect("failed to accept connection");
                let stream = tokio_tungstenite::accept_async(connection)
                        .await.expect("failed to handshake");
                
                $f(stream, client_index).await.expect("server function failed");
            }
        });

        ready_receive.await.expect("server is not ready");

        handle
    }};
}

/// Set up the client connection to the test server with the given port.
/// 
/// The WebSocket stream is then returned for use in the tests.
#[macro_export]
macro_rules! client_setup {
    ($p:literal) => {{
        let server_addr = format!("127.0.0.1:{}", $p);
        let tcp = tokio::net::TcpStream::connect(server_addr.clone()).await
                .expect("failed to connect");
        let (stream, _) = tokio_tungstenite::client_async(
                format!("ws://{}", server_addr), tcp).await
                .expect("client failed to connect");
        
        stream
    }};
}
