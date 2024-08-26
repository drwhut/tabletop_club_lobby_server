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
use crate::message::{LobbyControl, RoomCommand, RoomNotification, RoomRequest};
use crate::player::in_room::{PlayerInRoom, PlayerInRoomContext};
use crate::player::*;
use crate::room_code::RoomCode;

use nohash_hasher::IntMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::mem::drop;
use tokio::sync::broadcast::error::SendError;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, trace, warn};

/// The [`PlayerID`] of the host of a room.
pub const HOST_ID: PlayerID = 1;

/// The number of [`RoomRequest`]s that can be sent from players before they
/// would need to wait for the room to read the buffer.
const PLAYER_REQUEST_BUFFER_SIZE: usize = 10;

/// The number of [`RoomNotification`]s that can be sent to players before the
/// task needs to read them, at the risk of lagging and missing one.
const ROOM_NOTIFICATION_CAPACITY: usize = 50;

/// A type which keeps track of the players within a room, with the [`PlayerID`]
/// as the key, and the values being the [`PlayerInRoom`] instance, along with
/// a [`RoomNotification`] sender.
type PlayerMap = IntMap<PlayerID, PlayerTask>;

/// The data required to spawn a [`Room`] instance.
pub struct RoomContext {
    /// The unique [`RoomCode`] used to identify the new room.
    pub room_code: RoomCode,

    /// The WebSocket stream for the host of the room.
    pub host_stream: PlayerStream,

    /// A channel for the lobby to send new clients to join the room.
    pub new_client_receiver: mpsc::Receiver<PlayerStream>,

    /// A channel for the room to send control messages to the lobby.
    pub lobby_control_sender: mpsc::Sender<LobbyControl>,

    /// A watch channel for the server configuration.
    pub config_receiver: watch::Receiver<VariableConfig>,

    /// The shutdown signal receiver from the main thread.
    pub shutdown_signal: broadcast::Receiver<()>,
}

/// The room task, which allows players to communicate with each other.
///
/// When a player in the join queue has sent a request to create a room, an
/// instance of this task should be created and stored somewhere. The player
/// that created the room is made the host, and a [`PlayerInRoom`] task is
/// created to facilitate communication with the client.
///
/// Other clients can then join the same room by using the unique [`RoomCode`]
/// given by the lobby. All clients that were already in the room are notified
/// of the new player and their [`PlayerID`], which they can use to send WebRTC
/// messages to that player if they wish.
pub struct Room {
    handle: JoinHandle<()>,
}

impl Room {
    /// Spawn a new instance of [`Room`] with the given `context`.
    pub fn spawn(context: RoomContext) -> Self {
        Self {
            handle: tokio::spawn(Self::task(context)),
        }
    }

    /// Get the [`JoinHandle`] for this instance.
    pub fn handle(&mut self) -> &mut JoinHandle<()> {
        &mut self.handle
    }

    /// The task for handling the room, and the players within it.
    #[tracing::instrument(name="room", skip_all, fields(room_code = %context.room_code))]
    async fn task(mut context: RoomContext) {
        trace!("initialising");

        // The list of players currently in the room, along with a channel for
        // sending room notifications to the player.
        let mut player_map = PlayerMap::default();

        // Read the server configuration as it currently stands - it may be
        // updated later.
        let starting_config = *context.config_receiver.borrow_and_update();
        let mut max_players_per_room = starting_config.max_players_per_room;
        debug!(max_players_per_room, "read config");

        // Create channels for players to send us requests and closes.
        let (player_request_sender, mut player_request_receiver) =
            mpsc::channel(PLAYER_REQUEST_BUFFER_SIZE);
        let (player_close_sender, mut player_close_receiver) =
            mpsc::channel(PLAYER_REQUEST_BUFFER_SIZE);

        // Immediately add the host to the room.
        Self::on_connect(
            context.room_code,
            HOST_ID,
            context.host_stream,
            &mut player_map,
            player_request_sender.clone(),
            player_close_sender.clone(),
            context.config_receiver.clone(),
            context.shutdown_signal.resubscribe(),
        )
        .await;

        // Sending a close code to a client is done in a separate task, which
        // means we will need to keep track of them in the event that we need
        // to suddenly exit.
        let mut close_task_tracker = TaskTracker::new();

        info!("ready");

        loop {
            tokio::select! {
                res = context.new_client_receiver.recv() => match res {
                    Some(client_stream) => {
                        trace!("received new client from lobby");

                        // Check if the room is full first.
                        if player_map.len() < max_players_per_room {
                            // Make a new PlayerID for this client, that doesn't
                            // already exist in the room.
                            let mut player_id = Self::random_player_id();
                            while player_map.contains_key(&player_id) {
                                player_id = Self::random_player_id();
                            }

                            info!(player_id, "player joining");
                            Self::on_connect(
                                context.room_code,
                                player_id,
                                client_stream,
                                &mut player_map,
                                player_request_sender.clone(),
                                player_close_sender.clone(),
                                context.config_receiver.clone(),
                                context.shutdown_signal.resubscribe()
                            )
                            .await;
                        } else {
                            warn!("room is full, closing connection");
                            close_task_tracker.spawn(send_close(SendCloseContext {
                                client_stream,
                                close_code: CustomCloseCode::TooManyPlayers.into(),
                                client_id: None,
                            }));
                        }
                    },
                    None => {
                        // If we get here, this means the lobby is gone.
                        error!("new client sender dropped");
                        break;
                    }
                },

                res = player_request_receiver.recv() => match res {
                    Some(request) => {
                        debug!(
                            player_id = request.player_id,
                            command = %request.command,
                            "received request from player task"
                        );
                        match request.command {
                            RoomCommand::SealRoom => {
                                if Self::try_seal_room(
                                    request.player_id,
                                    context.room_code,
                                    &mut player_map,
                                    &mut player_request_receiver,
                                    &mut player_close_receiver,
                                    &mut close_task_tracker
                                )
                                .await {
                                    break;
                                }
                            },
                            RoomCommand::SendOffer(to_id, payload) => {
                                Self::try_send_message(
                                    request.player_id,
                                    to_id,
                                    MessageType::Offer,
                                    payload,
                                    &mut player_map
                                )
                                .await;
                            },
                            RoomCommand::SendAnswer(to_id, payload) => {
                                Self::try_send_message(
                                    request.player_id,
                                    to_id,
                                    MessageType::Answer,
                                    payload,
                                    &mut player_map
                                )
                                .await;
                            },
                            RoomCommand::SendCandidate(to_id, payload) => {
                                Self::try_send_message(
                                    request.player_id,
                                    to_id,
                                    MessageType::Candidate,
                                    payload,
                                    &mut player_map
                                )
                                .await;
                            },
                            RoomCommand::DropConnection => {
                                if request.player_id == HOST_ID {
                                    Self::on_close(
                                        RoomNotification::HostLeft,
                                        context.room_code,
                                        &mut player_map,
                                        &mut player_request_receiver,
                                        &mut player_close_receiver,
                                        &mut close_task_tracker
                                    )
                                    .await;
                                    break;
                                } else {
                                    Self::on_disconnect(
                                        request.player_id,
                                        &mut player_map,
                                        None
                                    )
                                    .await;
                                }
                            },
                        }
                    },
                    None => {
                        // Something's gone very wrong if we get here.
                        error!("all player request senders dropped");
                        break;
                    }
                },

                res = player_close_receiver.recv() => match res {
                    Some((player_id, player_stream, close_code)) => {
                        debug!(player_id, %close_code,
                            "received close request from player task");

                        if player_id == HOST_ID {
                            Self::on_close(
                                RoomNotification::HostLeft,
                                context.room_code,
                                &mut player_map,
                                &mut player_request_receiver,
                                &mut player_close_receiver,
                                &mut close_task_tracker
                            )
                            .await;
                            break;
                        } else {
                            Self::on_disconnect(
                                player_id,
                                &mut player_map,
                                Some((
                                    &mut close_task_tracker,
                                    context.room_code,
                                    player_stream,
                                    close_code))
                            )
                            .await;
                        }
                    },
                    None => {
                        // Something's gone very wrong if we get here.
                        error!("all close request senders dropped");
                        break;
                    }
                },

                Ok(()) = context.config_receiver.changed() => {
                    let new_config = *context.config_receiver.borrow_and_update();
                    max_players_per_room = new_config.max_players_per_room;
                    info!(max_players_per_room, "room config updated");

                    // If the number of players in the room is higher than the
                    // new maximum, then we need to kick players out until the
                    // maximum is satisfied.
                    let num_players = player_map.len();
                    if num_players > max_players_per_room {
                        warn!(num_players, "too many players now, kicking excess players");
                        let num_to_kick = num_players - max_players_per_room;
                        let kick_iter = player_map
                            .iter()
                            .filter(|(&id, _)| id != HOST_ID)
                            .take(num_to_kick);

                        for (player_id, player_task) in kick_iter {
                            warn!(player_id, "kicking player");
                            let noti = RoomNotification::Error(CustomCloseCode::TooManyPlayers.into());
                            if let Err(e) = player_task.send_notification(noti) {
                                error!(player_id, error = %e,
                                    "failed to send close notification to player");
                            }
                        }
                    }
                }

                // If we get the shutdown signal from the main thread, just let
                // all of the player instances drop without sending them close
                // messages.
                _ = context.shutdown_signal.recv() => {
                    break;
                }
            }
        }

        // Let the lobby know that the lobby is now sealed, meaning that it
        // should not allow any more clients to join the room.
        trace!("sending seal signal to the lobby");
        let control = LobbyControl::SealRoom(context.room_code);
        if let Err(e) = context.lobby_control_sender.send(control).await {
            error!(error = %e, "failed to send seal signal to the lobby");
        }

        // Before we wait for the player tasks to finish, we need to make sure
        // that the receivers for requests and closes are empty - this is to
        // ensure that the tasks are not stuck waiting to send requests, and
        // that ultimately we won't hang trying to await the handles.

        // By dropping the room's senders, the only senders left should be the
        // ones in the tasks. This way, we are guaranteed to get a `None` come
        // through at some point.
        drop(player_close_sender);
        drop(player_request_sender);

        trace!("making sure close channel is empty");
        loop {
            if player_close_receiver.recv().await.is_none() {
                break;
            }
        }

        trace!("making sure request channel is empty");
        loop {
            if player_request_receiver.recv().await.is_none() {
                break;
            }
        }

        // If there are any player tasks left, for example if we received the
        // shutdown signal, then we should wait for them to close gracefully.
        trace!("waiting for remaining player tasks to finish");
        for (player_id, mut player_task) in player_map.drain() {
            if let Err(e) = player_task.handle().await {
                error!(player_id, error = %e, "player task did not finish");
            }
            debug!(player_id, "player task finished");
        }

        // It's possible that a client was sent to us by the lobby just before
        // we sent the seal signal - if that's the case, we want to spawn a task
        // to send them a close code saying that the room has been sealed.
        trace!("checking for joining clients that we did not process");
        loop {
            match context.new_client_receiver.try_recv() {
                Ok(client_stream) => {
                    trace!("spawning close task for joining client");
                    close_task_tracker.spawn(send_close(SendCloseContext {
                        client_stream,
                        close_code: CustomCloseCode::RoomSealed.into(),
                        client_id: None,
                    }));
                }
                Err(e) => match e {
                    TryRecvError::Empty => {
                        trace!("no more joining clients to account for");
                        break;
                    }
                    TryRecvError::Disconnected => {
                        error!("new client sender dropped");
                        break;
                    }
                },
            }
        }

        // Wait for all remaining close tasks to finish.
        if !close_task_tracker.is_empty() {
            info!(
                num_tasks = close_task_tracker.len(),
                "waiting for connections to close"
            );
        }
        close_task_tracker.close();
        close_task_tracker.wait().await;

        // Send a closed signal to the lobby, now that we are really, actually,
        // for-realsies done here.
        trace!("sending close signal to the lobby");
        let control = LobbyControl::CloseRoom(context.room_code);
        if let Err(e) = context.lobby_control_sender.send(control).await {
            error!(error = %e, "failed to send close signal to the lobby");
        }

        info!("closed");
    }

    /// Try and pass a WebRTC message from one player in the room to another,
    /// in the form of a [`RoomNotification`].
    ///
    /// If the recipient does not exist, then the sender's connection is closed.
    ///
    /// TODO: Set a limit on the number of messages that can be sent from one
    /// player to each other player.
    async fn try_send_message(
        sender_id: PlayerID,
        receiver_id: PlayerID,
        message_type: MessageType,
        message_payload: String,
        player_map: &mut PlayerMap,
    ) {
        debug!(sender_id, receiver_id, %message_type, "attempting to send message");
        if let Some(sender_task) = player_map.get(&sender_id) {
            if let Some(receiver_task) = player_map.get(&receiver_id) {
                let message = match message_type {
                    MessageType::Offer => {
                        RoomNotification::OfferReceived(sender_id, message_payload)
                    }
                    MessageType::Answer => {
                        RoomNotification::AnswerReceived(sender_id, message_payload)
                    }
                    MessageType::Candidate => {
                        RoomNotification::CandidateReceived(sender_id, message_payload)
                    }
                };

                info!(receiver_id, %message, "sending message");
                if let Err(e) = receiver_task.send_notification(message) {
                    error!(receiver_id, error = %e, "failed to send message to receiver");
                }
            } else {
                error!(
                    sender_id,
                    receiver_id, "cannot send message, receiver does not exist"
                );

                let err = RoomNotification::Error(CustomCloseCode::InvalidDestination.into());
                trace!("sending close notification to sender");
                if let Err(e) = sender_task.send_notification(err) {
                    error!(sender_id, error = %e, "failed to send close notification to sender");
                }
            }
        } else {
            warn!(sender_id, "id does not exist in player map, ignoring");
        }
    }

    /// Try and seal the room on behalf of the player with the given `player_id`.
    ///
    /// If the request is successful, `true` is returned. In this case, you
    /// should break out of the main loop.
    ///
    /// If the player does not have permission to seal the room, the connection
    /// is closed, and `false` is returned.
    async fn try_seal_room(
        player_id: PlayerID,
        room_code: RoomCode,
        player_map: &mut PlayerMap,
        request_receiver: &mut mpsc::Receiver<RoomRequest>,
        close_receiver: &mut mpsc::Receiver<(PlayerID, PlayerStream, CloseCode)>,
        close_task_tracker: &mut TaskTracker,
    ) -> bool {
        debug!(player_id, "player is attempting to seal room");

        if player_id == HOST_ID {
            info!("host has sealed room");
            Self::on_close(
                RoomNotification::RoomSealed,
                room_code,
                player_map,
                request_receiver,
                close_receiver,
                close_task_tracker,
            )
            .await;

            true
        } else {
            // Attempt to send a close notification to the player that requested
            // to seal the room.
            if let Some(player_task) = player_map.get(&player_id) {
                warn!(
                    player_id,
                    "player attempted to seal room, closing connection"
                );

                let notification = RoomNotification::Error(CustomCloseCode::OnlyHostCanSeal.into());
                trace!("sending close notification to player");
                if let Err(e) = player_task.send_notification(notification) {
                    error!(player_id, error = %e, "failed to send close notification to player");
                }
            } else {
                warn!(player_id, "id that requested seal does not exist, ignoring");
            }

            false
        }
    }

    /// When a client joins the room, they need to be added to the player map,
    /// and all of the other clients need to be informed of their impending
    /// presence.
    async fn on_connect(
        room_code: RoomCode,
        player_id: PlayerID,
        player_stream: PlayerStream,
        player_map: &mut PlayerMap,
        request_sender: mpsc::Sender<RoomRequest>,
        close_sender: mpsc::Sender<(PlayerID, PlayerStream, CloseCode)>,
        config_receiver: watch::Receiver<VariableConfig>,
        shutdown_signal: broadcast::Receiver<()>,
    ) {
        trace!("sending notification of new player to existing players");
        Self::notify_all(RoomNotification::PlayerJoined(player_id), player_map).await;

        // Create a notification channel for this player specifically.
        let (notification_sender, notification_receiver) =
            broadcast::channel(ROOM_NOTIFICATION_CAPACITY);

        // Spawn a task for handling the client, and add them to the player map.
        debug!(player_id, "spawning player task");
        let player_task = PlayerTask::new(
            PlayerInRoomContext {
                client_stream: player_stream,
                room_code,
                player_id,
                other_ids: player_map.keys().map(|&id| id).collect(),
                room_request_sender: request_sender,
                room_close_sender: close_sender,
                room_notification_receiver: notification_receiver,
                config_receiver,
                shutdown_signal,
            },
            notification_sender,
        );

        // If there was already a player with the given ID, log an error and
        // forcefully abort the replaced task.
        if let Some(mut replaced_task) = player_map.insert(player_id, player_task) {
            error!(player_id, "id already exists in room, aborting old task");
            replaced_task.handle().abort();
        }
    }

    /// When a client that is not the host disconnects, the other players need
    /// to be informed of that player's departure, and the client needs to be
    /// removed from the player map.
    ///
    /// Optionally, a [`PlayerStream`] and [`CloseCode`] can be provided for
    /// spawning a [`send_close`] task.
    async fn on_disconnect(
        player_id: PlayerID,
        player_map: &mut PlayerMap,
        close: Option<(&mut TaskTracker, RoomCode, PlayerStream, CloseCode)>,
    ) {
        debug!(player_id, "player is disconnecting, removing from room");

        if let Some(mut player_task) = player_map.remove(&player_id) {
            trace!("waiting for player handle to finish");

            // Usually, we would need to make sure that the request and close
            // channels are empty before waiting for the task to finish, to
            // ensure that the task won't hang while waiting to send a request.
            // However, we can make an exception here, as this function should
            // be called as a result of one of these requests being received,
            // and that the task should have broken out of it's main loop after
            // sending the request.
            if let Err(e) = player_task.handle().await {
                error!(player_id, error = %e, "player task did not finish");
            }
        } else {
            error!(player_id, "cannot remove player, player does not exist");
        }

        if let Some((close_task_tracker, room_code, client_stream, close_code)) = close {
            info!(player_id, %close_code, "closing connection");
            close_task_tracker.spawn(send_close(SendCloseContext {
                client_stream,
                close_code,
                client_id: Some(ClientUniqueID::HasJoined {
                    room_code,
                    player_id,
                }),
            }));
        } else {
            info!(player_id, "connection dropped");
        }

        trace!("sending notification of leaving player to remaining players");
        Self::notify_all(RoomNotification::PlayerLeft(player_id), player_map).await;
    }

    /// Send the given `notification` to all players, and wait for each player
    /// task to finish. Each client stream is then gracefully closed with the
    /// corresponding [`CloseCode`].
    ///
    /// This function will clear the `player_map`, so it's best to break out of
    /// the main loop after!
    ///
    /// **NOTE:** The `notification` must cause the player task to send a close
    /// request, otherwise the function will hang!
    async fn on_close(
        notification: RoomNotification,
        room_code: RoomCode,
        player_map: &mut PlayerMap,
        request_receiver: &mut mpsc::Receiver<RoomRequest>,
        close_receiver: &mut mpsc::Receiver<(PlayerID, PlayerStream, CloseCode)>,
        close_task_tracker: &mut TaskTracker,
    ) {
        info!(%notification, "closing");

        // Keep track of which player tasks we still need to await.
        let mut to_await = IntMap::default();

        // Remove all of the players from the player map, and add them to the
        // await map. This way, we can keep track of which tasks have been
        // closed, and which ones are still active.
        trace!("sending close notifications");
        for (player_id, mut player_task) in player_map.drain() {
            debug!(player_id, "sending close notification to player");
            match player_task.send_notification(notification.clone()) {
                Ok(_) => {
                    trace!("adding player task to await map");
                    match to_await.insert(player_id, player_task) {
                        Some(mut task) => {
                            // This should not happen, but if it does abort the
                            // old task.
                            error!(player_id, "player task already in await map");
                            task.handle().abort();
                        }
                        None => {}
                    }
                }
                Err(_) => {
                    // If the notification failed to send, that can only mean
                    // that the receiver was dropped, which means that the
                    // player task ended early - therefore, we're safe to await
                    // the task now.
                    // NOTE: It's worth pointing out in the rare case that the
                    // receiver has lagged behind and missed notifications, then
                    // they will have sent a close request with an error close
                    // code anyway, so we can proceed as normal.
                    warn!(player_id, "notification receiver dropped, waiting to close");
                    if let Err(e) = player_task.handle().await {
                        error!(player_id, error = %e, "player task did not finish");
                    }
                }
            }
        }

        // Keep track of which clients we need to spawn a close task for.
        let mut to_close = Vec::new();

        // It's possible that at least one of the player tasks is waiting to
        // send a request or a close to the room, if either of the channels is
        // currently full. Because of this, we need to drain the channels and
        // check through them for close or drop requests.
        while !to_await.is_empty() {
            debug!(n = to_await.len(), "waiting for close requests");

            tokio::select! {
                res = request_receiver.recv() => match res {
                    Some(request) => match request.command {
                        RoomCommand::DropConnection => {
                            let player_id = request.player_id;
                            debug!(player_id, "received drop request");

                            if let Some(mut player_task) = to_await.remove(&player_id) {
                                trace!("waiting for player task to close");
                                if let Err(e) = player_task.handle().await {
                                    error!(player_id, error = %e, "player task did not finish");
                                }
                            } else {
                                warn!(player_id, "player task was not in await map, ignoring");
                            }
                        },

                        // Ignore all other requests at this stage.
                        _ => {}
                    },
                    None => {
                        error!("all player request senders dropped");
                        break;
                    },
                },

                res = close_receiver.recv() => match res {
                    Some(close_req) => {
                        let player_id = close_req.0;
                        debug!(player_id, close_code = %close_req.2,
                            "received close request");
                        to_close.push(close_req);

                        if let Some(mut player_task) = to_await.remove(&player_id) {
                            trace!("waiting for player task to close");
                            if let Err(e) = player_task.handle().await {
                                error!(player_id, error = %e, "player task did not finish");
                            }
                        } else {
                            warn!(player_id, "player task was not in await map, ignoring");
                        }
                    },
                    None => {
                        error!("all close request senders dropped");
                        break;
                    },
                }
            }
        }

        // Now that all of the player tasks have been closed, we need to spawn
        // close tasks for the clients that we need to send a close code to,
        // and wait for an echo from.
        trace!("spawning close tasks");
        for (player_id, client_stream, close_code) in to_close {
            debug!(player_id, %close_code, "spawning close task for player");
            close_task_tracker.spawn(send_close(SendCloseContext {
                client_stream,
                close_code,
                client_id: Some(ClientUniqueID::HasJoined {
                    room_code,
                    player_id,
                }),
            }));
        }
    }

    /// Send the given `notification` to all existing players.
    ///
    /// If any notifications fail to send, implying that the receiver has been
    /// dropped, then that receiver's task is aborted.
    async fn notify_all(notification: RoomNotification, player_map: &mut PlayerMap) {
        debug!(%notification, "sending notification to all players");

        // Required so we can borrow the hash map again as mutable.
        let player_id_list = player_map.keys().map(|&id| id).collect::<Vec<PlayerID>>();
        for player_id in player_id_list {
            if let Entry::Occupied(entry) = player_map.entry(player_id) {
                let player_task = entry.get();

                debug!(player_id, %notification, "sending notification to player");
                if let Err(e) = player_task.send_notification(notification.clone()) {
                    error!(player_id, error = %e, "failed to send notification to player");

                    // Since the receiver has been dropped, we can assume that
                    // the task has failed somehow. So we need to kill it.
                    // For being a failure.
                    trace!("aborting player task");
                    let mut player_task = entry.remove();
                    player_task.handle().abort();
                }
            } else {
                warn!(player_id, "vacant entry in player map, ignoring");
            }
        }
    }

    /// Generate a random [`PlayerID`] for a non-host player.
    fn random_player_id() -> PlayerID {
        fastrand::u32(2..)
    }
}

/// A wrapper around the [`PlayerInRoom`] task, with extra metadata.
#[derive(Debug)]
struct PlayerTask {
    /// The task itself, which handles communication with the client.
    task: PlayerInRoom,

    /// A channel for sending notifications to the task.
    notification_sender: broadcast::Sender<RoomNotification>,
}

impl PlayerTask {
    /// Create a new [`PlayerTask`].
    ///
    /// The `context` is needed to spawn the [`PlayerInRoom`] instance, and the
    /// `notification_sender` allows the room to send notifications to the
    /// player task, so it can react to events. It must belong to the same
    /// channel as the receiver provided in `context`.
    pub fn new(
        context: PlayerInRoomContext,
        notification_sender: broadcast::Sender<RoomNotification>,
    ) -> Self {
        Self {
            task: PlayerInRoom::spawn(context),
            notification_sender,
        }
    }

    /// Get a mutable reference to the task's handle, which can be used to
    /// either await the task's completion, or to abort the task.
    pub fn handle(&mut self) -> &mut JoinHandle<()> {
        self.task.handle()
    }

    /// Send a [`RoomNotification`] to the player's task.
    pub fn send_notification(
        &self,
        notification: RoomNotification,
    ) -> Result<(), SendError<RoomNotification>> {
        // We don't need to know how many receivers the channel has, since we
        // know the answer will always be one.
        self.notification_sender.send(notification).map(|_| ())
    }
}

/// The types of WebRTC messages that can be sent between players.
#[derive(Debug)]
enum MessageType {
    Offer,
    Answer,
    Candidate,
}

impl fmt::Display for MessageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offer => write!(f, "offer"),
            Self::Answer => write!(f, "answer"),
            Self::Candidate => write!(f, "candidate"),
        }
    }
}
