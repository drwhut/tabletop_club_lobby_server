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
use crate::message::{LobbyCommand, LobbyControl};
use crate::player::joining::*;
use crate::player::*;
use crate::room::{Room, RoomContext};
use crate::room_code::RoomCode;

use nohash_hasher::IntMap;
use std::mem::drop;
use std::time::Duration;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, trace, warn};

/// The number of [`LobbyRequest`]s that can be stored in the [`lobby_task`]
/// before [`PlayerJoining`] tasks need to wait to send them.
const LOBBY_REQUEST_CHANNEL_BUFFER_SIZE: usize = 50;

/// The number of [`PlayerStream`]s that can be stored on the way to the client
/// joining a given room, before they start getting dropped due to the channel
/// being full.
const NEW_CLIENT_CHANNEL_BUFFER_SIZE: usize = 10;

/// The number of [`LobbyControl`]s that can be sent by rooms before the room
/// tasks need to wait to send them.
const LOBBY_CONTROL_CHANNEL_BUFFER_SIZE: usize = 50;

/// The data needed to start the [`lobby_task`].
pub struct LobbyContext {
    /// A channel for receiving connections once they have been established.
    ///
    /// Sending WebSocket channels to this receiver will add them to the lobby
    /// as a waiting player.
    pub connection_receiver: mpsc::Receiver<PlayerStream>,

    /// A watch channel for the server configuration.
    pub config_receiver: watch::Receiver<VariableConfig>,

    /// The shutdown signal from the main thread.
    pub shutdown_signal: broadcast::Receiver<()>,
}

/// Keeps track of active rooms, and players waiting to create or join them.
#[tracing::instrument(name = "lobby", skip_all)]
pub async fn lobby_task(mut context: LobbyContext) {
    trace!("initialising");

    // Create a channel for player instances to send us lobby requests.
    let (req_sender, mut req_receiver) = mpsc::channel(LOBBY_REQUEST_CHANNEL_BUFFER_SIZE);

    // Create a channel for room instances to send up lobby control signals.
    let (control_sender, mut control_receiver) = mpsc::channel(LOBBY_CONTROL_CHANNEL_BUFFER_SIZE);

    // The list of players that are currently waiting to create or join a lobby.
    let mut join_queue = IntMap::<HandleID, PlayerJoining>::default();

    // The list of rooms that are currently active.
    // NOTE: Instead of using the room code directly as the key, it is converted
    // into a 4-byte integer, so that we can take advantage of faster hashing.
    let mut room_map = IntMap::<u32, RoomTask>::default();

    // Read the server configuration as it currently stands - it may be updated
    // later.
    let starting_config = *context.config_receiver.borrow_and_update();
    let mut max_rooms = starting_config.max_rooms;
    let mut player_queue_capacity = starting_config.player_queue_capacity;
    let join_room_time_limit_secs = starting_config.join_room_time_limit_secs;

    debug!(
        max_rooms,
        player_queue_capacity, join_room_time_limit_secs, "read config"
    );

    // Create dedicated [`Duration`] structures for time limits.
    let mut join_room_time_limit = Duration::from_secs(join_room_time_limit_secs);

    // Every so often we may need to terminate a client's connection early.
    // We do this in separate tasks so they don't clog up the main lobby task,
    // so we need to keep track of them.
    let close_task_tracker = TaskTracker::new();

    info!("started");

    // Wait for incoming connections and messages.
    loop {
        tokio::select! {
            // Has a connection just been established with a client?
            res = context.connection_receiver.recv() => {
                if let Some(client_stream) = res {
                    trace!("connection received");

                    let maybe_close_code = {
                        let num_players_in_queue = join_queue.len();
                        debug!(n = num_players_in_queue,
                            "checking the number of players in the join queue");

                        if num_players_in_queue < player_queue_capacity {
                            trace!("join queue has space, adding new player");
                            None
                        } else {
                            warn!("join queue is full, closing connection");
                            Some(CustomCloseCode::JoinQueueFull.into())
                        }
                    };

                    if let Some(close_code) = maybe_close_code {
                        debug!(%close_code, "spawning task to close connection");

                        close_task_tracker.spawn(send_close(SendCloseContext {
                            client_stream,
                            close_code,
                            client_id: None,
                        }));
                    } else {
                        // Generate a random HandleID that isn't already in
                        // use.
                        let mut handle_id = fastrand::u32(..);
                        while join_queue.contains_key(&handle_id) {
                            handle_id = fastrand::u32(..);
                        }

                        debug!(handle_id, "created handle id for client");

                        // Spawn the player instance, and add them to the
                        // join queue.
                        let join_task = PlayerJoining::spawn(PlayerJoiningContext {
                            handle_id,
                            client_stream,
                            lobby_request_sender: req_sender.clone(),
                            max_wait_time: join_room_time_limit
                        });

                        info!(handle_id, "adding player to join queue");
                        join_queue.insert(handle_id, join_task);
                    }
                } else {
                    // All senders have been dropped - we will never get
                    // another incoming client.
                    panic!("all connection senders have been dropped");
                }
            },

            // Has a client in the join queue sent us a request?
            res = req_receiver.recv() => {
                if let Some((request, client_stream)) = res {
                    info!(handle_id = request.handle_id, command = %request.command);

                    trace!("removing player from join queue");
                    if join_queue.remove(&request.handle_id).is_none() {
                        error!(handle_id = request.handle_id,
                            "handle id not found in join queue");
                    }

                    match request.command {
                        LobbyCommand::CreateRoom => {
                            // Create a new room, and have the player be the
                            // room's host.
                            let num_rooms = room_map.len();
                            debug!(n = num_rooms,
                                "checking the number of active rooms");

                            if num_rooms < max_rooms {
                                trace!("setting up new room");
                                // Create a channel for sending new clients to
                                // the room.
                                let (new_client_sender, new_client_receiver)
                                    = mpsc::channel(NEW_CLIENT_CHANNEL_BUFFER_SIZE);

                                // Generate a room code that isn't already in
                                // use, and that doesn't contain profanity.
                                let mut room_code = RoomCode::random();
                                while
                                    room_map.contains_key(&room_code.into()) ||
                                    room_code.is_profanity()
                                {
                                    room_code = RoomCode::random();
                                }

                                info!(%room_code, "creating room");
                                let room_task = RoomTask::new(
                                    RoomContext {
                                        room_code,
                                        host_stream: client_stream,
                                        new_client_receiver,
                                        lobby_control_sender: control_sender.clone(),
                                        config_receiver: context.config_receiver.clone(),
                                        shutdown_signal: context.shutdown_signal.resubscribe(),
                                    },
                                    new_client_sender
                                );

                                trace!("adding room to the room map");
                                if let Some(mut old_task) = room_map.insert(room_code.into(), room_task) {
                                    error!(%room_code,
                                        "room task already exists, aborting old task");
                                    old_task.handle().abort();
                                }
                            } else {
                                warn!("too many rooms, closing connection");
                                close_task_tracker.spawn(send_close(SendCloseContext {
                                    client_stream,
                                    close_code: CustomCloseCode::TooManyRooms.into(),
                                    client_id: Some(ClientUniqueID::IsJoining(request.handle_id)),
                                }));
                            }
                        },
                        LobbyCommand::JoinRoom(room_code) => {
                            // Check if the room exists, and have the player
                            // join it.
                            if let Some(room_task) = room_map.get(&room_code.into()) {
                                if !room_task.is_sealed {
                                    info!(handle_id = request.handle_id, %room_code,
                                        "sending new client to join room");

                                    match room_task.send_new_client(client_stream) {
                                        Ok(_) => {
                                            trace!("sent new client to room");
                                        },
                                        Err(e) => {
                                            let (stream, close_code) = match e {
                                                TrySendError::Full(s) => {
                                                    warn!(%room_code,
                                                        "join channel is full, closing connection");
                                                    (s, CloseCode::Again)
                                                },
                                                TrySendError::Closed(s) => {
                                                    error!(%room_code,
                                                        "join receiver dropped");
                                                    (s, CustomCloseCode::RoomSealed.into())
                                                },
                                            };

                                            close_task_tracker.spawn(send_close(SendCloseContext {
                                                client_stream: stream,
                                                close_code,
                                                client_id: Some(ClientUniqueID::IsJoining(request.handle_id)),
                                            }));
                                        }
                                    }
                                } else {
                                    warn!(%room_code,
                                        "room is sealed, closing connection");
                                    close_task_tracker.spawn(send_close(SendCloseContext {
                                        client_stream,
                                        close_code: CustomCloseCode::RoomSealed.into(),
                                        client_id: Some(ClientUniqueID::IsJoining(request.handle_id)),
                                    }));
                                }
                            } else {
                                warn!(%room_code,
                                    "room does not exist, closing connection");
                                close_task_tracker.spawn(send_close(SendCloseContext {
                                    client_stream,
                                    close_code: CustomCloseCode::RoomDoesNotExist.into(),
                                    client_id: Some(ClientUniqueID::IsJoining(request.handle_id)),
                                }));
                            }
                        },
                        LobbyCommand::CloseConnection(close_code) => {
                            // Spawn a send_close task to properly close the
                            // connection.
                            trace!("closing connection");
                            close_task_tracker.spawn(send_close(SendCloseContext {
                                client_stream,
                                close_code,
                                client_id: Some(ClientUniqueID::IsJoining(request.handle_id)),
                            }));
                        },
                        LobbyCommand::DropConnection => {
                            // Not a lot to do here, other than to let the
                            // stream go out of scope.
                            trace!("dropping connection");
                        },
                    };
                } else {
                    // All senders have been dropped - we will never get another
                    // request again.
                    panic!("all request senders have been dropped");
                }
            },

            // Has one of the rooms sent us a control signal?
            // NOTE: Since Tokio randomly decides which futures to poll first in
            // select! macros, it's possible that a player could join a room at
            // the same time it is sealed.
            // If Tokio decides to poll the join request first, then either it
            // gets sent through the join channel and maybe(?) dealt with by the
            // room depending on how far it is into being closed, or the join
            // channel is closed and we send back a RoomSealed close code anyway.
            res = control_receiver.recv() => {
                if let Some(control) = res {
                    debug!(%control, "received control signal from room");

                    match control {
                        LobbyControl::SealRoom(room_code) => {
                            info!(%room_code, "sealing room");
                            if let Some(room_task) = room_map.get_mut(&room_code.into()) {
                                room_task.is_sealed = true;
                            } else {
                                error!(%room_code,
                                    "cannot seal room that does not exist");
                            }
                        },
                        LobbyControl::CloseRoom(room_code) => {
                            info!(%room_code, "removing room");
                            if let Some(mut room_task) = room_map.remove(&room_code.into()) {
                                // Since it gave us the close signal, we can
                                // safely assume that it's task is done.
                                trace!("waiting for room task to finish");
                                if let Err(e) = room_task.handle().await {
                                    error!(error = %e,
                                        "room task did not finish");
                                }
                            } else {
                                error!(%room_code,
                                    "cannot close room that does not exist");
                            }
                        },
                    }
                } else {
                    // All senders have been dropped - we will never get another
                    // control signal again.
                    panic!("all control signal senders have been dropped");
                }
            },

            // Has the server configuration changed?
            Ok(()) = context.config_receiver.changed() => {
                let new_config = context.config_receiver.borrow_and_update();

                max_rooms = new_config.max_rooms;
                player_queue_capacity = new_config.player_queue_capacity;
                let join_room_time_limit_secs = new_config.join_room_time_limit_secs;
                info!(
                    max_rooms,
                    player_queue_capacity,
                    join_room_time_limit_secs, "lobby config updated"
                );

                // Update the [`Duration`]s for timings.
                join_room_time_limit = Duration::from_secs(join_room_time_limit_secs);

                // If `max_rooms` has been decreased, then we may need to shut
                // down some rooms to conform to the new maximum.
                let num_rooms = room_map.len();
                if num_rooms > max_rooms {
                    warn!(num_rooms, "too many rooms now, closing excess rooms");
                    let num_to_close = num_rooms - max_rooms;
                    let to_close: Vec<u32> = room_map
                        .keys()
                        .map(|&rc| rc)
                        .take(num_to_close)
                        .collect();

                    for room_code_int in to_close {
                        let room_code: RoomCode = room_code_int
                            .try_into()
                            .unwrap_or(RoomCode::default());
                        warn!(%room_code, "closing room");

                        if let Some(mut room_task) = room_map.remove(&room_code_int) {
                            // TODO: Should this be done gracefully, given how
                            // rare and disruptive this senario is?
                            room_task.handle().abort();
                        } else {
                            error!(%room_code, "room does not exist in room map");
                        }
                    }
                }
            },

            // Have we been asked to shut down by the main thread?
            _ = context.shutdown_signal.recv() => {
                break;
            }
        }
    }

    // In order to shut down gracefully, we need to wait for all join and room
    // tasks to finish - but they could be waiting to send requests or control
    // signals to us if the channels are full. Therefore, we need to drain the
    // channels fully before we can continue.
    drop(req_sender);
    drop(control_sender);

    trace!("waiting for all lobby request senders to drop");
    loop {
        if req_receiver.recv().await.is_none() {
            break;
        }
    }

    trace!("waiting for all control signal senders to drop");
    loop {
        if control_receiver.recv().await.is_none() {
            break;
        }
    }

    // Now we can safely await all of the remaining tasks.
    for (handle_id, mut join_task) in join_queue.drain() {
        debug!(handle_id, "waiting for join task to finish");
        if let Err(e) = join_task.handle().await {
            error!(error = %e, "join task did not finish");
        }
    }

    for (room_code_int, mut room_task) in room_map.drain() {
        let room_code: RoomCode = room_code_int.try_into().unwrap_or(RoomCode::default());

        debug!(%room_code, "waiting for room task to finish");
        if let Err(e) = room_task.handle().await {
            error!(error = %e, "room task did not finish");
        }
    }

    // If we are still trying to gracefully close connections, let them play out
    // before we exit.
    if !close_task_tracker.is_empty() {
        info!(
            num_tasks = close_task_tracker.len(),
            "waiting for connections to close"
        );
    }
    close_task_tracker.close();
    close_task_tracker.wait().await;

    info!("stopped");
}

/// A wrapper around the [`Room`] task.
#[derive(Debug)]
struct RoomTask {
    /// The task itself, which handles communication with clients in the room.
    task: Room,

    /// A channel for sending new clients to join the room.
    new_client_sender: mpsc::Sender<PlayerStream>,

    /// A flag indicating if the room is sealed.
    ///
    /// This should be set to `true` once the room gives the
    /// [`LobbyControl::SealRoom`] signal, indicating that no new players should
    /// be able to join the room, and that the room is in the process of closing.
    pub is_sealed: bool,
}

impl RoomTask {
    /// Create a new [`RoomTask`].
    ///
    /// The `context` is needed to spawn the [`Room`] instance, and the
    /// `new_client_sender` allows the lobby to send new clients to the room
    /// task, so they can join the room. It must belong to the same channel as
    /// the receiver provided in `context`.
    pub fn new(context: RoomContext, new_client_sender: mpsc::Sender<PlayerStream>) -> Self {
        Self {
            task: Room::spawn(context),
            new_client_sender,
            is_sealed: false,
        }
    }

    /// Get a mutable reference to the task's handle, which can be used to
    /// either await the task's completion, or to abort the task.
    pub fn handle(&mut self) -> &mut JoinHandle<()> {
        self.task.handle()
    }

    /// Send a [`PlayerStream`] to the room's task.
    pub fn send_new_client(
        &self,
        new_client: PlayerStream,
    ) -> Result<(), TrySendError<PlayerStream>> {
        // NOTE: We use `try_send` here instead of the normal send, because we
        // don't need to await it. This is a requirement because of the fact
        // there there is also an MPSC channel going from the room to the lobby,
        // which can lead to the rare situation where the room is trying to send
        // a control signal to the lobby, while the lobby is trying to send a
        // new client to the room.
        // However, this introduces the possibility that even though the request
        // is valid, a player might not join the room due to the channel being
        // full. In this case, we want to make the channel big enough to make
        // this unlikely, and we should close the connection gracefully with a
        // relevant close code.
        self.new_client_sender.try_send(new_client)
    }
}

// NOTE: This module is pretty high-level, so it is tested by the integration
// tests, rather than with unit tests.
