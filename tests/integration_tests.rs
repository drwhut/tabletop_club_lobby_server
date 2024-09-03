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

use tabletop_club_lobby_server::close_code::{CloseCode, CustomCloseCode};
use tabletop_club_lobby_server::config::VariableConfig;
use tabletop_club_lobby_server::player::PlayerStream;
use tabletop_club_lobby_server::room_code::RoomCode;

use futures_util::{SinkExt, StreamExt};
use tokio::fs;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

async fn start_server(port: u16) -> (JoinHandle<()>, broadcast::Sender<()>) {
    let server_addr = format!("127.0.0.1:{}", port);
    let tcp_listener = TcpListener::bind(server_addr)
        .await
        .expect("failed to create listener");

    let (shutdown_sender, shutdown_receiver) = broadcast::channel(1);

    let context = tabletop_club_lobby_server::ServerContext {
        config_file_path: "__server__.toml".into(),
        tcp_listener,
        tls_acceptor: None,
        shutdown_signal: shutdown_receiver,
    };

    let handle = tokio::spawn(tabletop_club_lobby_server::server_task(context));
    (handle, shutdown_sender)
}

async fn start_client(port: u16) -> PlayerStream {
    let server_addr = format!("127.0.0.1:{}", port);
    let tcp = TcpStream::connect(server_addr.clone())
        .await
        .expect("failed to connect");

    let server_req = format!("ws://{}", server_addr);
    let maybe_tls = tokio_tungstenite::MaybeTlsStream::Plain(tcp);
    let (stream, _) = tokio_tungstenite::client_async(server_req, maybe_tls)
        .await
        .expect("client failed to connect");

    stream
}

macro_rules! send_text {
    ($s:ident, $m:expr) => {
        $s.send(Message::Text(String::from($m)))
            .await
            .expect("failed to send");
    };
}

macro_rules! assert_msg {
    ($s:ident, $m:expr) => {
        let res = $s.next().await.expect("stream ended early");
        let msg = res.expect("error receiving message from server");
        assert_eq!(msg, Message::Text(String::from($m)));
    };
}

macro_rules! assert_id {
    ($s:ident) => {{
        let res = $s.next().await.expect("stream ended early");
        let msg = res.expect("error receiving message from server");
        match msg {
            Message::Text(mut msg) => {
                let mut id_str = msg.split_off(3);
                assert_eq!(msg, "I: ");

                let newline = id_str.split_off(id_str.len() - 1);
                assert_eq!(newline, "\n");

                let id = id_str.parse::<u32>().expect("id is not u32");
                id
            }
            _ => panic!("expected message type to be text"),
        }
    }};
}

macro_rules! assert_room {
    ($s:ident) => {{
        let res = $s.next().await.expect("stream ended early");
        let msg = res.expect("error receiving message from server");
        match msg {
            Message::Text(mut msg) => {
                let mut room_str = msg.split_off(3);
                assert_eq!(msg, "J: ");

                let newline = room_str.split_off(room_str.len() - 1);
                assert_eq!(newline, "\n");

                let code: RoomCode = room_str.try_into().expect("expected valid room code");
                code
            }
            _ => panic!("expected message type to be text"),
        }
    }};
}

macro_rules! assert_ping {
    ($s:ident) => {
        let res = $s.next().await.expect("stream ended early");
        let msg = res.expect("error receiving message from server");
        assert_eq!(msg, Message::Ping(vec!()));
    };
}

macro_rules! assert_close {
    ($s:ident, $c:expr) => {
        let res = $s.next().await.expect("stream ended early");
        let msg = res.expect("error receiving message from server");
        match msg {
            Message::Close(close) => {
                let close = close.expect("expected close code");
                assert_eq!(close.code, $c);
            }
            _ => panic!("expected message type to be close"),
        }
    };
}

macro_rules! assert_end {
    ($s:ident) => {
        assert!($s.next().await.is_none());
    };
}

#[tokio::test]
async fn test_lobby() {
    // TODO: Once metrics have been added, test them fully here.

    // Make a new config, and save it so the server reads it.
    // TODO: Find a way to test changing these values on the fly using paused
    // time. Issues start coming up when using WebSocket connections, though.
    let test_config = VariableConfig {
        max_message_size: 200,
        max_payload_size: 100,
        max_players_per_room: 2,
        max_rooms: 1,
        player_queue_capacity: 1,
        max_message_count: 10,
        join_room_time_limit_secs: 5,
        ping_interval_secs: 7,
        response_time_limit_secs: 30,
        room_time_limit_mins: 10,
    };

    test_config
        .write_config_file("__server__.toml")
        .await
        .expect("failed to write config file");

    let (server_handle, shutdown_sender) = start_server(13000).await;

    // Have a client join the queue, but not send a request in time.
    let mut timeout_stream = start_client(13000).await;

    // While that client is in the join queue, have another client try to join
    // it while it is full.
    sleep(Duration::from_secs(2)).await;
    let mut join_queue_full_stream = start_client(13000).await;
    assert_close!(
        join_queue_full_stream,
        CustomCloseCode::JoinQueueFull.into()
    );
    assert_end!(join_queue_full_stream);

    sleep(Duration::from_secs(3)).await;
    assert_close!(timeout_stream, CustomCloseCode::DidNotJoinRoom.into());
    assert_end!(timeout_stream);

    // Test sending a close code while in the join queue.
    let mut close_stream = start_client(13000).await;
    close_stream
        .close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        }))
        .await
        .expect("failed to send close");
    close_stream
        .next()
        .await
        .unwrap()
        .expect_err("expected connection to end");

    // Test that a client can create a room.
    let mut host_stream = start_client(13000).await;
    send_text!(host_stream, "J: \n");
    assert_msg!(host_stream, "I: 1\n");
    assert_room!(host_stream);
    assert_ping!(host_stream);

    // Test that a client can't create another room, since we have reached the
    // maximum.
    let mut no_room_stream = start_client(13000).await;
    send_text!(no_room_stream, "J: \n");
    assert_close!(no_room_stream, CustomCloseCode::TooManyRooms.into());
    assert_end!(no_room_stream);

    // Test that rooms in the lobby are closed properly.
    send_text!(host_stream, "S: \n");
    assert_close!(host_stream, CustomCloseCode::RoomSealed.into());
    assert_end!(host_stream);

    // Test joining a room that doesn't exist.
    let mut no_room_stream = start_client(13000).await;
    send_text!(no_room_stream, "J: WHAT\n");
    assert_close!(no_room_stream, CustomCloseCode::RoomDoesNotExist.into());
    assert_end!(no_room_stream);

    let mut host_stream = start_client(13000).await;
    send_text!(host_stream, "J: \n");
    assert_msg!(host_stream, "I: 1\n");
    let room_code = assert_room!(host_stream);
    assert_ping!(host_stream);

    // Test joining a room that exists.
    let mut client_stream = start_client(13000).await;
    let join_msg = format!("J: {}\n", room_code);
    send_text!(client_stream, join_msg.clone());
    let client_id = assert_id!(client_stream);
    assert_msg!(client_stream, "N: 1\n");
    assert_msg!(client_stream, join_msg.clone());
    assert_ping!(client_stream);

    // Test that the host is made aware of the new client.
    assert_msg!(host_stream, format!("N: {}\n", client_id));

    // Test joining a room that is full.
    let mut no_room_stream = start_client(13000).await;
    send_text!(no_room_stream, join_msg);
    assert_close!(no_room_stream, CustomCloseCode::TooManyPlayers.into());
    assert_end!(no_room_stream);

    // TODO: Find a way to test joining a room when it is sealed, but not closed.
    // TODO: Find a way to test joining a room when the channel is full.

    shutdown_sender
        .send(())
        .expect("failed to send shutdown signal");
    server_handle.await.expect("task was aborted");

    fs::remove_file("__server__.toml")
        .await
        .expect("failed to remove config file");
}
