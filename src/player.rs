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
use crate::config::VariableConfig;
use crate::message::{LobbyCommand, LobbyRequest, RoomCommand, RoomRequest, RoomNotification};
use crate::room_code::RoomCode;

use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use std::fmt;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout, Duration};
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
/// initialise it, then the library will automatically send an echo frame back,
/// after which the connection should be dropped.
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
                            trace!("received echo was the expected value");
                        } else {
                            warn!(
                                expected = %context.close_code,
                                got = %close_code_received,
                                "received echo was not the expected value"
                            );
                        }
                    },
                    Err(_) => {} // Call should output error log.
                },
                Err(_) => {
                    warn!("did not receive echo in time, dropping connection");
                }
            }
        },
        Err(e) => {
            warn!(error = %e, "failed to send close code, dropping connection");
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
                            warn!("received echo did not contain close frame, dropping connection");
                            return Err(());
                        }
                    },

                    // If the message is not a close frame, then we need
                    // to keep reading until we get one.
                    _ => {}
                },
                Err(e) => {
                    warn!(error = %e, "error receiving echo, dropping connection");
                    return Err(());
                },
            }
            None => {
                error!("client stream ended early");
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
                                    error!(request = %req, "client sent request before joining a room");
                                    LobbyCommand::CloseConnection(CustomCloseCode::NotInRoom.into())
                                }
                            },
                            Err(e) => {
                                error!(error = %e, "error parsing request from client");
                                LobbyCommand::CloseConnection(parse_error_to_close_code(e))
                            },
                        },
                        Message::Binary(_) => {
                            error!("received binary message from client");
                            LobbyCommand::CloseConnection(CloseCode::Unsupported)
                        },
                        Message::Ping(_) => {
                            error!("received ping from client");
                            LobbyCommand::CloseConnection(CloseCode::Protocol)
                        },
                        Message::Pong(_) => {
                            error!("received pong from client before joining a room");
                            LobbyCommand::CloseConnection(CloseCode::Protocol)
                        },
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
                        },
                        Message::Frame(_) => {
                            // Should never get this while reading.
                            error!("received raw frame from stream");
                            LobbyCommand::DropConnection
                        },
                    },
                    Err(e) => {
                        error!(error = %e, "error receiving request from client");
                        match websocket_error_to_close_code(e) {
                            Some(close_code) => LobbyCommand::CloseConnection(close_code),
                            None => LobbyCommand::DropConnection,
                        }
                    },
                },
                None => {
                    error!("client stream ended early");
                    LobbyCommand::DropConnection
                },
            },
            Err(_) => {
                error!("did not receive a request from the client in time");
                LobbyCommand::CloseConnection(CustomCloseCode::DidNotJoinRoom.into())
            },
        };

        let request = LobbyRequest { handle_id: context.handle_id, command };
        match context.lobby_request_sender.send((request, context.client_stream)).await {
            Ok(_) => trace!("sent request to the lobby"),
            Err(e) => error!(error = %e, "lobby request receiver dropped"),
        };
    }
}

/// The data required to spawn a [`PlayerInRoom`] instance.
pub struct PlayerInRoomContext {
    /// The WebSocket stream for this player.
    pub client_stream: PlayerStream,

    /// The room the player has joined.
    pub room_code: RoomCode,

    /// The ID given to this player within the room.
    pub player_id: PlayerID,

    /// The IDs of the other players in the room at the time of joining.
    /// 
    /// **NOTE:** This list is only used once at the start of the instance. The
    /// "true" list of players is stored in the room itself.
    pub other_ids: Vec<PlayerID>,

    /// A channel for sending requests to the room.
    pub room_request_sender: mpsc::Sender<RoomRequest>,

    /// A channel for sending the `client_stream` to the room, so that it can
    /// gracefully close the connection with the given close code.
    pub room_close_sender: mpsc::Sender<(PlayerID, PlayerStream, CloseCode)>,

    /// A channel for receiving notifications from the room.
    pub room_notification_receiver: mpsc::Receiver<RoomNotification>,

    /// A watch channel for the server configuration.
    pub config_receiver: watch::Receiver<VariableConfig>,

    /// The shutdown signal receiver from the main thread.
    pub shutdown_signal: broadcast::Receiver<()>,
}

/// An instance which handles communication with a client after they have joined
/// a room within the lobby.
/// 
/// The instance first sends the client their [`PlayerID`], followed by the IDs
/// of the other clients that are in the room. After that, the [`RoomCode`] is
/// sent to the client, whether they hosted a room, or joined an existing one.
/// 
/// Once the data has been sent, it is then up to the client whether they want
/// to establish a peer-to-peer connection with the other players or not. As of
/// Tabletop Club v0.2.0, the host of the room is the one that initiates the
/// process of establishing connections with incoming clients.
pub struct PlayerInRoom {
    handle: JoinHandle<()>,
}

/// A helper macro for sending a request to the room to close this player's
/// connection gracefully.
macro_rules! close_connection {
    ($c:ident, $f:expr) => {
        let room_req = ($c.player_id, $c.client_stream, $f);

        if let Err(e) = $c.room_close_sender.send(room_req).await {
            error!(error = %e, "room close receiver dropped");
        }
    }
}

/// A helper macro for sending a request to the room to immediately drop this
/// player's connection.
macro_rules! drop_connection {
    ($c:ident) => {
        let room_req = RoomRequest {
            player_id: $c.player_id,
            command: RoomCommand::DropConnection
        };

        if let Err(e) = $c.room_request_sender.send(room_req).await {
            error!(error = %e, "room request receiver dropped");
        }
    }
}

/// A helper macro for sending messages to the client, and exiting the task if
/// there was an error sending the message.
macro_rules! send_msg {
    ($c:ident, $s:expr, $e:literal) => {
        // NOTE: This call will produce a lot of trace logs.
        if let Err(e) = $c.client_stream.send($s).await {
            error!(error = %e, $e);

            // Let the room know that we can no longer send messages reliably.
            drop_connection!($c);

            return;
        }
    }
}

/// A helper macro for sending requests to the room, and exiting the task if
/// there was an error sending the request.
macro_rules! send_req {
    ($c:ident, $r:expr) => {
        let room_req = RoomRequest {
            player_id: $c.player_id,
            command: $r
        };

        // NOTE: The room task will log the request in more detail.
        trace!("sending request to the room task");

        if let Err(e) = $c.room_request_sender.send(room_req).await {
            error!(error = %e, "room request receiver dropped");

            // The task can no longer function properly if the room task has
            // suddenly disappeared, so forcefully stop the task.
            return;
        }
    }
}

impl PlayerInRoom {
    /// Spawn a new instance of [`PlayerInRoom`] with the given `context`.
    pub fn spawn(context: PlayerInRoomContext) -> Self {
        Self {
            handle: tokio::spawn(Self::task(context)),
        }
    }

    /// Get the [`JoinHandle`] for this instance.
    pub fn handle(&mut self) -> &mut JoinHandle<()> {
        &mut self.handle
    }

    /// Handle communication with this player's client.
    #[tracing::instrument(name="player", skip_all, fields(room = %context.room_code, player_id = context.player_id))]
    async fn task(mut context: PlayerInRoomContext) {
        trace!("sending room details to the player");

        send_msg!(context, Message::Text(format!("I: {}\n", context.player_id)),
                "failed to send player id to new player");
        
        for other_id in context.other_ids {
            send_msg!(context, Message::Text(format!("N: {}\n", other_id)),
                    "failed to send other player id to new player");
        }

        send_msg!(context, Message::Text(format!("J: {}\n", context.room_code)),
                "failed to send room code to new player");
        
        // Read the server configuration as it currently stands - it may be
        // updated later.
        let starting_config = *context.config_receiver.borrow_and_update();
        let ping_interval_secs = starting_config.ping_interval_secs;
        let response_time_limit_secs = starting_config.response_time_limit_secs;

        debug!(ping_interval_secs, response_time_limit_secs, "read config");

        // Create dedicated [`Duration`] structures for time limits.
        let ping_interval_duration = Duration::from_secs(ping_interval_secs);
        let response_time_limit = Duration::from_secs(response_time_limit_secs);

        // We need to send a ping every so often, so that we know whether the
        // connection is still alive or not. The interval depends on the server
        // configuration, which can change during run-time.
        let mut ping_interval = interval(ping_interval_duration);

        // Start an interval which emits a tick when the client has not sent a
        // pong message in time, and reset it when they do send one.
        let mut timeout_interval = interval(response_time_limit);
        timeout_interval.tick().await; // Ignore first initial tick.

        // Each time we send a ping to the client, we expect a pong message
        // back - but we don't expect the client to send more pongs than we have
        // sent them pings.
        let mut num_expected_pongs: usize = 0;

        info!("setup complete");

        loop {
            tokio::select! {
                res = context.client_stream.next() => match res {
                    Some(res) => match res {
                        Ok(msg) => match msg {
                            Message::Text(text) => match parse_player_request(&text) {
                                Ok(req) => match req {
                                    PlayerRequest::Seal => {
                                        send_req!(context, RoomCommand::SealRoom);
                                    },
                                    PlayerRequest::Offer(target_id, payload) => {
                                        send_req!(context, RoomCommand::SendOffer(
                                            target_id,
                                            payload
                                        ));
                                    },
                                    PlayerRequest::Answer(target_id, payload) => {
                                        send_req!(context, RoomCommand::SendAnswer(
                                            target_id,
                                            payload
                                        ));
                                    },
                                    PlayerRequest::Candidate(target_id, payload) => {
                                        send_req!(context, RoomCommand::SendCandidate(
                                            target_id,
                                            payload
                                        ));
                                    },

                                    _ => {
                                        error!(request = %req, "client sent request while in a room");
                                        close_connection!(context,
                                                CustomCloseCode::AlreadyInRoom.into());
                                        break;
                                    }
                                },
                                Err(e) => {
                                    error!(error = %e, "error parsing request from client");
                                    let close_code = parse_error_to_close_code(e);
                                    close_connection!(context, close_code);
                                    break;
                                },
                            },
                            Message::Binary(_) => {
                                error!("received binary message from client");
                                close_connection!(context, CloseCode::Unsupported);
                                break;
                            },
                            Message::Ping(_) => {
                                // Only the server is allowed to send ping
                                // messages.
                                error!("received unexpected ping from client");
                                close_connection!(context, CloseCode::Protocol);
                                break;
                            },
                            Message::Pong(_) => {
                                if num_expected_pongs > 0 {
                                    trace!("received pong from client");
                                    num_expected_pongs -= 1;

                                    // Reset the timeout timer, and ignore the
                                    // first initial tick.
                                    timeout_interval.reset_immediately();
                                    timeout_interval.tick().await;
                                } else {
                                    error!("received unexpected pong from client");
                                    close_connection!(context, CloseCode::Protocol);
                                    break;
                                }
                            },
                            Message::Close(maybe_close_frame) => {
                                if let Some(close_frame) = maybe_close_frame {
                                    if close_frame.code == CloseCode::Normal {
                                        info!("client is closing connection");
                                    } else {
                                        warn!(code = %close_frame, "client sent close message");
                                    }
                                } else {
                                    warn!("client sent close message");
                                }

                                // The library should have automatically
                                // sent an echo frame back for us.
                                drop_connection!(context);
                                break;
                            },
                            Message::Frame(_) => {
                                // Should never receive a raw frame!
                                error!("received raw frame from stream");
                                drop_connection!(context);
                                break;
                            },
                        },
                        Err(e) => {
                            error!(error = %e, "error receiving message from client");
                            let maybe_close_code = websocket_error_to_close_code(e);

                            if let Some(close_code) = maybe_close_code {
                                close_connection!(context, close_code);
                            } else {
                                drop_connection!(context);
                            }

                            break;
                        }
                    },
                    None => {
                        error!("client stream ended early");
                        drop_connection!(context);
                        break;
                    }
                },

                res = context.room_notification_receiver.recv() => match res {
                    Some(notification) => {
                        debug!(%notification, "received notification from room");
                        match notification {
                            RoomNotification::PlayerJoined(id) => {
                                send_msg!(context,
                                        Message::Text(format!("N: {}\n", id)),
                                        "failed to send id of joining player");
                            },
                            RoomNotification::PlayerLeft(id) => {
                                send_msg!(context,
                                        Message::Text(format!("D: {}\n", id)),
                                        "failed to send id of leaving player");
                            },
                            RoomNotification::OfferReceived(from, payload) => {
                                let msg = format!("O: {}\n{}", from, payload);
                                send_msg!(context, Message::Text(msg),
                                        "failed to send player's offer");
                            },
                            RoomNotification::AnswerReceived(from, payload) => {
                                let msg = format!("A: {}\n{}", from, payload);
                                send_msg!(context, Message::Text(msg),
                                        "failed to send player's answer");
                            },
                            RoomNotification::CandidateReceived(from, payload) => {
                                let msg = format!("C: {}\n{}", from, payload);
                                send_msg!(context, Message::Text(msg),
                                        "failed to send player's candidate");
                            },
                            RoomNotification::HostLeft => {
                                trace!("host has left room, closing connection");
                                close_connection!(context,
                                        CustomCloseCode::HostDisconnected.into());
                                break;
                            },
                            RoomNotification::RoomSealed => {
                                trace!("room has been sealed, closing connection");
                                close_connection!(context,
                                        CustomCloseCode::RoomSealed.into());
                                break;
                            },
                            RoomNotification::Error(code) => {
                                error!(%code, "an error occured, closing connection");
                                close_connection!(context, code);
                                break;
                            },
                        }
                    },
                    None => {
                        // If the sender got dropped, that means the room task
                        // no longer exists, which means - we're boned.
                        error!("room notification sender dropped");
                        return;
                    }
                },

                // NOTE: The first ping will occur immediately.
                _ = ping_interval.tick() => {
                    trace!("sending ping");
                    send_msg!(context, Message::Ping(vec![]), "failed to send ping");
                    num_expected_pongs += 1;
                },

                _ = timeout_interval.tick() => {
                    warn!("connection timed out with client");
                    drop_connection!(context);
                    break;
                },

                Ok(()) = context.config_receiver.changed() => {
                    let new_config = *context.config_receiver.borrow_and_update();

                    let ping_interval_secs = new_config.ping_interval_secs;
                    let response_time_limit_secs = new_config.response_time_limit_secs;

                    info!(
                        ping_interval_secs,
                        response_time_limit_secs, "player config updated"
                    );

                    // Update the [`Duration`]s for timings.
                    let ping_interval_duration = Duration::from_secs(ping_interval_secs);
                    let response_time_limit = Duration::from_secs(response_time_limit_secs);

                    // Update the interval futures with the new durations.
                    ping_interval = interval(ping_interval_duration);
                    timeout_interval = interval(response_time_limit);

                    // Ignore first tick of `timeout_interval`, so as to not
                    // time out the client immediately after. However, a ping
                    // will be sent to the client immediately afterwards.
                    timeout_interval.tick().await;
                },

                // If we get the shutdown signal from the main thread, let the
                // connection drop.
                _ = context.shutdown_signal.recv() => {
                    break;
                }
            }
        }

        trace!("task stopped");
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
    ExpectedPayload,
    InvalidCommand,
    InvalidArgument,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoNewline => write!(f, "no new line character in message"),
            Self::NoSpace => write!(f, "no space character between command and argument"),
            Self::UnexpectedPayload => write!(f, "received a payload when we did not expect one"),
            Self::ExpectedPayload => write!(f, "did not receive a payload when we expected one"),
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
                        if payload.is_empty() {
                            return Err(ParseError::ExpectedPayload);
                        }

                        Ok(PlayerRequest::Offer(player_id, String::from(payload)))
                    } else {
                        Err(ParseError::InvalidArgument)
                    }
                },
                "A:" => {
                    if let Ok(player_id) = argument.parse::<PlayerID>() {
                        if payload.is_empty() {
                            return Err(ParseError::ExpectedPayload);
                        }

                        Ok(PlayerRequest::Answer(player_id, String::from(payload)))
                    } else {
                        Err(ParseError::InvalidArgument)
                    }
                },
                "C:" => {
                    if let Ok(player_id) = argument.parse::<PlayerID>() {
                        if payload.is_empty() {
                            return Err(ParseError::ExpectedPayload);
                        }

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
        ParseError::ExpectedPayload => CustomCloseCode::InvalidFormat.into(),
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

    use tokio::time::sleep;

    async fn send_close_server(stream: PlayerStream, index: u16, _shutdown: broadcast::Receiver<()>) -> Result<(), ()> {
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
        let (handle, _) = crate::server_setup!(10000, 1, send_close_server);

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

    async fn player_joining_server(stream: PlayerStream, index: u16, _shutdown: broadcast::Receiver<()>) -> Result<(), ()> {
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
        let (handle, _) = crate::server_setup!(10001, 19, player_joining_server);

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

    async fn player_in_room_server(stream: PlayerStream, index: u32, shutdown: broadcast::Receiver<()>) -> Result<(), mpsc::error::SendError<RoomNotification>> {
        let room_code_int = ((65 << 24) | (65 << 16) | (65 << 8) | (65 << 0))
                + (index % 26);
        let room_code: RoomCode = room_code_int.try_into().unwrap();

        let player_id = match index {
            2 => 1024,
            3 => 6732,
            4 => 7,
            8 => 2048,
            _ => 1,
        };
        let other_ids = match index {
            2 => vec!(1),
            3 => vec!(1, 4261),
            4 => vec!(6, 5, 4, 3, 2, 1),
            8 => vec!(1, 512),
            _ => vec!(),
        };

        let (room_request_sender, mut room_request_receiver) = mpsc::channel(1);
        let (room_close_sender, mut room_close_receiver) = mpsc::channel(1);
        let (room_notification_sender, room_notification_receiver) =
                mpsc::channel(1);
        
        let mut config = VariableConfig::default();
        config.ping_interval_secs = 2;
        config.response_time_limit_secs = 5;

        let (config_sender, config_receiver) = watch::channel(config);

        let mut task = PlayerInRoom::spawn(PlayerInRoomContext {
            client_stream: stream,
            room_code,
            player_id,
            other_ids,
            room_request_sender,
            room_close_sender,
            room_notification_receiver,
            config_receiver,
            shutdown_signal: shutdown,
        });

        match index {
            0 => {
                // Send notifications at specific times to make sure that the
                // correct number of pings are being sent inbetween messages.
                sleep(Duration::from_millis(500)).await;
                let noti = RoomNotification::PlayerJoined(500);
                room_notification_sender.send(noti).await?;

                sleep(Duration::from_secs(5)).await;
                let noti = RoomNotification::PlayerJoined(5);
                room_notification_sender.send(noti).await?;

                sleep(Duration::from_secs(4)).await;
                let noti = RoomNotification::PlayerJoined(9);
                room_notification_sender.send(noti).await?;
            }
            1 => {
                // Wait a bit for the initial ping to be sent, then update the
                // config so that another ping gets sent at 0.5s. We also want
                // to send messages so the client can check how many pings got
                // through.
                sleep(Duration::from_millis(250)).await;
                let noti = RoomNotification::PlayerJoined(250);
                room_notification_sender.send(noti).await?;

                sleep(Duration::from_millis(250)).await;
                config.ping_interval_secs = 1;
                config.response_time_limit_secs = 8;
                config_sender.send(config).expect("failed to send config");

                sleep(Duration::from_millis(6250)).await;
                let noti = RoomNotification::PlayerJoined(6750);
                room_notification_sender.send(noti).await?;
            },
            5 => {
                sleep(Duration::from_millis(500)).await;

                // Send out a range of notifications to see if the client
                // receives them in the correct format.
                // NOTE: 0 is an invalid index for a PlayerID, but this could
                // change in the future. It's also the room's responsibility to
                // make sure player IDs are valid.
                let noti = RoomNotification::PlayerJoined(0);
                room_notification_sender.send(noti).await?;

                let noti = RoomNotification::PlayerJoined(145265);
                room_notification_sender.send(noti).await?;

                let noti = RoomNotification::PlayerJoined(std::u32::MAX);
                room_notification_sender.send(noti).await?;

                // Again, this would be invalid in a real senario, but it's the
                // room's responsibility to send valid notifications.
                let noti = RoomNotification::PlayerLeft(1);
                room_notification_sender.send(noti).await?;

                let noti = RoomNotification::PlayerLeft(4246273);
                room_notification_sender.send(noti).await?;

                let noti = RoomNotification::PlayerLeft(std::u32::MAX);
                room_notification_sender.send(noti).await?;

                let noti = RoomNotification::OfferReceived(200, "hello world!".into());
                room_notification_sender.send(noti).await?;

                let noti = RoomNotification::OfferReceived(std::u32::MAX,
                        "max integer".into());
                room_notification_sender.send(noti).await?;

                let noti = RoomNotification::AnswerReceived(1000,
                        "lol\n2nd line!".into());
                room_notification_sender.send(noti).await?;

                // Send a message just under the payload limit of 16 MiB.
                let long = "?".repeat(16777211);
                let noti = RoomNotification::CandidateReceived(1, long);
                room_notification_sender.send(noti).await?;

                // If the host left, the task should stop and send back a close
                // request.
                room_notification_sender.send(RoomNotification::HostLeft).await?;
            },
            6 => {
                sleep(Duration::from_millis(500)).await;
                room_notification_sender.send(RoomNotification::RoomSealed).await?;
            },
            7 => {
                sleep(Duration::from_millis(500)).await;
                let noti = RoomNotification::Error(CloseCode::Abnormal);
                room_notification_sender.send(noti).await?;
            },
            8 => {
                sleep(Duration::from_millis(500)).await;
                let noti = RoomNotification::Error(CustomCloseCode::InvalidCommand.into());
                room_notification_sender.send(noti).await?;
            },
            25 => {
                // A series of valid requests should come through here.
                let req = room_request_receiver.recv().await.unwrap();
                assert_eq!(req, RoomRequest{
                    player_id: 1,
                    command: RoomCommand::SealRoom
                });

                let req = room_request_receiver.recv().await.unwrap();
                assert_eq!(req, RoomRequest{
                    player_id: 1,
                    command: RoomCommand::SendOffer(0, "\\o/".into())
                });

                let req = room_request_receiver.recv().await.unwrap();
                assert_eq!(req, RoomRequest{
                    player_id: 1,
                    command: RoomCommand::SendAnswer(4294967295, "boi".into())
                });

                let req = room_request_receiver.recv().await.unwrap();
                assert_eq!(req, RoomRequest{
                    player_id: 1,
                    command: RoomCommand::SendCandidate(1, "done!".into())
                });
            },
            _ => {}
        }

        // Should we have gotten a request from the player task at the end?
        let maybe_expected_req = match index {
            0 => Some(RoomRequest {
                player_id: 1,
                command: RoomCommand::DropConnection
            }),
            1 => Some(RoomRequest {
                player_id: 1,
                command: RoomCommand::DropConnection
            }),
            11 => Some(RoomRequest {
                player_id: 1,
                command: RoomCommand::DropConnection
            }),
            12 => Some(RoomRequest {
                player_id: 1,
                command: RoomCommand::DropConnection
            }),
            25 => Some(RoomRequest {
                player_id: 1,
                command: RoomCommand::DropConnection
            }),
            _ => None
        };

        assert_eq!(room_request_receiver.recv().await, maybe_expected_req);

        // Should we have gotten a close request from the task at the end?
        let maybe_expected_close = match index {
            2 => Some((1024, CloseCode::Protocol)),
            4 => Some((7, CloseCode::Protocol)),
            5 => Some((1, CustomCloseCode::HostDisconnected.into())),
            6 => Some((1, CustomCloseCode::RoomSealed.into())),
            7 => Some((1, CloseCode::Abnormal)),
            8 => Some((2048, CustomCloseCode::InvalidCommand.into())),
            9 => Some((1, CloseCode::Invalid)),
            10 => Some((1, CloseCode::Unsupported)),
            13 => Some((1, CustomCloseCode::InvalidFormat.into())),
            14 => Some((1, CustomCloseCode::InvalidFormat.into())),
            15 => Some((1, CustomCloseCode::InvalidCommand.into())),
            16 => Some((1, CustomCloseCode::InvalidDestination.into())),
            17 => Some((1, CustomCloseCode::InvalidDestination.into())),
            18 => Some((1, CustomCloseCode::InvalidDestination.into())),
            19 => Some((1, CustomCloseCode::InvalidDestination.into())),
            20 => Some((1, CustomCloseCode::InvalidFormat.into())),
            21 => Some((1, CustomCloseCode::InvalidFormat.into())),
            22 => Some((1, CloseCode::Size)),
            23 => Some((1, CustomCloseCode::AlreadyInRoom.into())),
            24 => Some((1, CustomCloseCode::AlreadyInRoom.into())),
            _ => None
        };

        match room_close_receiver.recv().await {
            Some(close) => {
                let expected_close = maybe_expected_close
                        .expect("task gave close request unexpectedly");

                assert_eq!(close.0, expected_close.0);
                // There is no way to check the stream was the same from here:
                // This will be checked in integration tests.
                assert_eq!(close.2, expected_close.1);
            },
            None => assert!(maybe_expected_close.is_none())
        };

        task.handle().await.expect("task was aborted");
        
        Ok(())
    }

    macro_rules! assert_msg {
        ($s:ident, $m:expr) => {
            let res = $s.next().await.expect("stream ended early");
            let msg = res.expect("error receiving message from server");
            assert_eq!(msg, Message::Text(String::from($m)));
        }
    }

    macro_rules! assert_ping {
        ($s:ident) => {
            let res = $s.next().await.expect("stream ended early");
            let msg = res.expect("error receiving message from server");
            assert_eq!(msg, Message::Ping(vec!()));
        }
    }

    macro_rules! assert_end {
        ($s:ident) => {
            $s.next().await.unwrap().expect_err("expected connection to be dropped");
        }
    }

    #[tokio::test]
    async fn player_in_room() {
        let (handle, shutdown_send) = crate::server_setup!(10002, 26,
                player_in_room_server);
        
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAA\n");
        assert_ping!(stream); // 0s
        assert_msg!(stream, "N: 500\n"); // 0.5s

        // Check that the correct number of pings are sent to the client.
        sleep(Duration::from_secs(4)).await;
        assert_ping!(stream); // 2s
        assert_ping!(stream); // 4s
        assert_msg!(stream, "N: 5\n"); // 5s

        // Check that the automatic pongs have reset the timeout timer.
        sleep(Duration::from_secs(4)).await;
        assert_ping!(stream); // 6s
        assert_ping!(stream); // 8s
        assert_msg!(stream, "N: 9\n"); // 9s

        // Check that the timeout timer works.
        sleep(Duration::from_secs(6)).await;
        // NOTE: Unfortunately, the library flushing pings from the server is
        // not consistent. That, plus the fact that the select! macro selects
        // branches at random, means that the number of pings we will receive
        // here is not deterministic. We've already shown that the server sends
        // pings at the correct rate, so just wait until we get an error in
        // the stream representing that the connection has closed.
        loop {
            if let Err(_) = stream.next().await.unwrap() {
                break;
            }
        }

        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAB\n");
        assert_ping!(stream); // 0s

        // Test if the server config update works as expected. It should change
        // the rate of ping messages, as well as reset the timeout timer.
        // NOTE: This also tests that the server allows pongs to be sent in
        // batches, as long as the number of them is not excessive.
        sleep(Duration::from_secs(7)).await;
        assert_msg!(stream, "N: 250\n"); // 0.25s
        assert_ping!(stream); // 0.5s
        assert_ping!(stream); // 1.5s
        assert_ping!(stream); // 2.5s
        assert_ping!(stream); // 3.5s
        assert_ping!(stream); // 4.5s
        assert_ping!(stream); // 5.5s
        assert_ping!(stream); // 6.5s
        assert_msg!(stream, "N: 6750\n"); // 6.75s

        // Check that the new timeout works.
        sleep(Duration::from_secs(9)).await;
        loop { // Same issue as above.
            if let Err(_) = stream.next().await.unwrap() {
                break;
            }
        }

        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1024\n");
        assert_msg!(stream, "N: 1\n");
        assert_msg!(stream, "J: AAAC\n");
        assert_ping!(stream);

        // Test that sending one too many pongs closes the connection.
        stream.send(Message::Pong(vec!())).await.expect("failed to send pong");
        stream.send(Message::Pong(vec!())).await.expect("failed to send pong");
        assert_end!(stream);

        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 6732\n");
        assert_msg!(stream, "N: 1\n");
        assert_msg!(stream, "N: 4261\n");
        assert_msg!(stream, "J: AAAD\n");
        assert_ping!(stream);

        // Test that sending the shutdown signal closes the connection.
        // NOTE: No requests should be made to the room from this.
        shutdown_send.send(()).expect("failed to send shutdown signal");
        assert_end!(stream);

        {
            let mut stream = crate::client_setup!(10002);
            assert_msg!(stream, "I: 7\n");
            assert_msg!(stream, "N: 6\n");
            assert_msg!(stream, "N: 5\n");
            assert_msg!(stream, "N: 4\n");
            assert_msg!(stream, "N: 3\n");
            assert_msg!(stream, "N: 2\n");
            assert_msg!(stream, "N: 1\n");
            assert_msg!(stream, "J: AAAE\n");
            assert_ping!(stream);
        }

        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAF\n");
        assert_ping!(stream);

        // Test the message that is received by the client with each
        // notification type.
        assert_msg!(stream, "N: 0\n");
        assert_msg!(stream, "N: 145265\n");
        assert_msg!(stream, "N: 4294967295\n");
        assert_msg!(stream, "D: 1\n");
        assert_msg!(stream, "D: 4246273\n");
        assert_msg!(stream, "D: 4294967295\n");
        assert_msg!(stream, "O: 200\nhello world!");
        assert_msg!(stream, "O: 4294967295\nmax integer");
        assert_msg!(stream, "A: 1000\nlol\n2nd line!");

        let valid_long = format!("C: 1\n{}", "?".repeat(16777211));
        assert_msg!(stream, valid_long);
        assert_end!(stream);

        // Some notifications will result in close requests, rather than direct
        // messages.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAG\n");
        assert_ping!(stream);
        assert_end!(stream);

        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAH\n");
        assert_ping!(stream);
        assert_end!(stream);

        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 2048\n");
        assert_msg!(stream, "N: 1\n");
        assert_msg!(stream, "N: 512\n");
        assert_msg!(stream, "J: AAAI\n");
        assert_ping!(stream);
        assert_end!(stream);

        // Test sending an invalid UTF-8 message.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAJ\n");
        assert_ping!(stream);
        let invalid_utf8 = unsafe {
            String::from_utf8_unchecked(vec![56, 123, 213, 85, 7])
        };
        stream.send(Message::Text(invalid_utf8)).await.expect("failed to send");
        assert_end!(stream);

        // Test sending a binary message.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAK\n");
        assert_ping!(stream);
        stream.send(Message::Binary(vec!(0, 1, 2))).await.expect("failed to send");
        assert_end!(stream);

        // Test sending close messages.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAL\n");
        assert_ping!(stream);
        stream.close(Some(CloseFrame{ code: CloseCode::Normal, reason: "".into() }))
                .await.expect("failed to send close");
        assert_end!(stream);

        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAM\n");
        assert_ping!(stream);
        stream.close(Some(CloseFrame{ code: CloseCode::Error, reason: "".into() }))
                .await.expect("failed to send close");
        assert_end!(stream);

        // Test sending a message that doesn't have a newline in it.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAN\n");
        assert_ping!(stream);
        stream.send(Message::Text("O: 1yo!".into())).await.expect("failed to send");
        assert_end!(stream);

        // Test sending a message without a space in it.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAO\n");
        assert_ping!(stream);
        stream.send(Message::Text("S:\n".into())).await.expect("failed to send");
        assert_end!(stream);

        // Test sending a message with an invalid command.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAP\n");
        assert_ping!(stream);
        stream.send(Message::Text("X: 2\nwhy".into())).await.expect("failed to send");
        assert_end!(stream);

        // Test sending a message with a PlayerID that is not an integer.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAQ\n");
        assert_ping!(stream);
        stream.send(Message::Text("A: dave\nhi!".into())).await.expect("failed to send");
        assert_end!(stream);

        // Test sending a message with a PlayerID that is blank.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAR\n");
        assert_ping!(stream);
        stream.send(Message::Text("O: \nwhut".into())).await.expect("failed to send");
        assert_end!(stream);

        // Test sending a message with a PlayerID that is negative.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAS\n");
        assert_ping!(stream);
        stream.send(Message::Text("C: -7\nuh oh".into())).await.expect("failed to send");
        assert_end!(stream);

        // Test sending a message with a PlayerID that is too big.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAT\n");
        assert_ping!(stream);
        stream.send(Message::Text("O: 4294967296\n>u32".into())).await.expect("failed to send");
        assert_end!(stream);

        // Test sending a seal request with a payload.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAU\n");
        assert_ping!(stream);
        stream.send(Message::Text("S: \npayload!".into())).await.expect("failed to send");
        assert_end!(stream);

        // Test sending a WebRTC message without a payload.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAV\n");
        assert_ping!(stream);
        stream.send(Message::Text("C: 20\n".into())).await.expect("failed to send");
        assert_end!(stream);

        // Test sending a WebRTC message with a payload that is slightly too big.
        let slightly_too_long = format!("A: 4294967295\n{}", "*".repeat(16777203));
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAW\n");
        assert_ping!(stream);
        stream.send(Message::Text(slightly_too_long)).await
                .expect_err("message should not have been received");
        assert_end!(stream);

        // Test creating a room while already in a room.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAX\n");
        assert_ping!(stream);
        stream.send(Message::Text("J: \n".into())).await.expect("failed to send");
        assert_end!(stream);

        // Test joining a room while already in a room.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAY\n");
        assert_ping!(stream);
        stream.send(Message::Text("J: BEAN\n".into())).await.expect("failed to send");
        assert_end!(stream);

        // Test valid requests.
        let mut stream = crate::client_setup!(10002);
        assert_msg!(stream, "I: 1\n");
        assert_msg!(stream, "J: AAAZ\n");
        assert_ping!(stream);
        stream.send(Message::Text("S: \n".into())).await.expect("failed to send");
        // NOTE: 0 is an invalid PlayerID, but the room checks this.
        stream.send(Message::Text("O: 0\n\\o/".into())).await.expect("failed to send");
        stream.send(Message::Text("A: 4294967295\nboi".into())).await.expect("failed to send");
        stream.send(Message::Text("C: 1\ndone!".into())).await.expect("failed to send");
        stream.close(None).await.expect("failed to send close message");
        assert_end!(stream);

        handle.await.expect("server was aborted");
    }
}
