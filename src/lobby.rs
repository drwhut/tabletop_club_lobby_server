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

use crate::config::VariableConfig;
use crate::player::{HandleID, Player, PlayerCloseCode, PlayerStream};

use futures_util::{SinkExt, StreamExt};
use nohash_hasher::IntMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{client, Message};
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, trace, warn};

/// The amount of time to wait for echo messages when closing connections.
const WAIT_FOR_CLOSE_ECHO_DURATION: Duration = Duration::from_secs(5);

/// The data needed to start the lobby task.
pub struct LobbyContext {
    /// A channel for receiving connections once they have been established.
    ///
    /// Sending WebSocket channels to this receiver will add them to the lobby
    /// as a waiting player.
    pub connection_receiver: mpsc::Receiver<(PlayerStream, SocketAddr)>,

    /// A watch channel for the server configuration.
    pub config_receiver: watch::Receiver<VariableConfig>,

    /// The shutdown signal from the main thread.
    pub shutdown_signal: broadcast::Receiver<()>,
}

/// Keeps track of active rooms, and players waiting to create or join them.
pub struct Lobby {
    handle: JoinHandle<()>,
}

impl Lobby {
    /// Spawn a new lobby task with the given `context`.
    pub fn spawn(context: LobbyContext) -> Self {
        Self {
            handle: tokio::spawn(Self::task(context)),
        }
    }

    /// Get the [`JoinHandle`] for the lobby task.
    pub fn handle(&mut self) -> &mut JoinHandle<()> {
        &mut self.handle
    }

    /// The main lobby task, which runs in its own thread.
    #[tracing::instrument(name = "lobby", skip_all)]
    async fn task(mut context: LobbyContext) {
        trace!("creating structures");

        // The list of players that are currently waiting to create or join a
        // lobby.
        let mut join_queue = IntMap::<HandleID, Player>::default();

        // For each remote address, keep track of the player handles they have
        // spawned. We will use this to limit the number of player instances
        // that individual remote origins can create.
        let mut origin_handle_map = HashMap::<SocketAddr, Vec<HandleID>>::default();

        // Read the server configuration as it currently stands - it may be
        // updated later.
        let starting_config = *context.config_receiver.borrow_and_update();
        let mut max_players_per_address = starting_config.max_players_per_address;
        let mut player_queue_capacity = starting_config.player_queue_capacity;

        debug!(
            max_players_per_address,
            player_queue_capacity, "read config"
        );

        // Every so often we may need to terminate a client's connection early.
        // We do this in separate tasks so they don't clog up the main lobby
        // task, so we need to keep track of them.
        let close_task_tracker = TaskTracker::new();

        info!("started");

        // Wait for incoming connections and messages.
        loop {
            tokio::select! {
                // Has a connection just been established with a client?
                res = context.connection_receiver.recv() => {
                    if let Some((client_stream, client_addr)) = res {
                        trace!("connection received");
                        let mut maybe_close_code: Option<CloseCode> = None;

                        // Check this address hasn't created too many players.
                        let handle_list = origin_handle_map.entry(client_addr).or_insert(Vec::new());
                        let addr_conn_count = handle_list.len();
                        debug!(n = addr_conn_count, "checking existing connections from this address");

                        if addr_conn_count >= max_players_per_address {
                            warn!("too many connections from this address");
                            maybe_close_code = Some(PlayerCloseCode::TooManyConnections.into());
                        }

                        // Check that the join queue isn't full.
                        if join_queue.len() >= player_queue_capacity {
                            warn!("player queue is full");
                            maybe_close_code = Some(PlayerCloseCode::JoinQueueFull.into());
                        }

                        if let Some(close_code) = maybe_close_code {
                            // Spawn a separate task for closing the stream.
                            trace!("spawning task to close connection");
                            close_task_tracker.spawn(Self::send_close(client_stream, close_code));
                        } else {
                            // Generate a random HandleID that isn't already in
                            // use.
                            let mut handle_id = fastrand::u32(..);
                            while join_queue.contains_key(&handle_id) {
                                handle_id = fastrand::u32(..);
                            }

                            debug!(handle_id, "created handle id for client");

                            // Make a note that this handle ID has come from
                            // this address.
                            handle_list.push(handle_id);
                        }
                    } else {
                        // All senders have been dropped - we will never get
                        // another incoming client.
                        error!("all connection senders have been dropped");
                        break;
                    }
                },

                // Has the server configuration changed?
                Ok(()) = context.config_receiver.changed() => {
                    let new_config = context.config_receiver.borrow_and_update();

                    max_players_per_address = new_config.max_players_per_address;
                    player_queue_capacity = new_config.player_queue_capacity;
                    info!(max_players_per_address, player_queue_capacity, "lobby config updated");

                    // TODO: Kick players out if values have been lowered.
                },

                // Have we been asked to shut down by the main thread?
                _ = context.shutdown_signal.recv() => {
                    break;
                }
            }
        }

        // If we are still trying to gracefully close connections, let them play
        // out before we exit.
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

    /// Send a close message to the client, and wait a certain amount of time
    /// for the client's echo message.
    /// TODO: Move to player struct?
    #[tracing::instrument(name = "lobby_send_close", skip_all)]
    async fn send_close(mut stream: PlayerStream, close_code: CloseCode) {
        let msg = Message::Close(Some(CloseFrame {
            code: close_code,
            reason: "".into(),
        }));

        debug!(%close_code, "sending");
        let expect_echo = match stream.send(msg).await {
            Ok(()) => true,
            Err(e) => {
                warn!(error = %e, "failed to send close code, dropping connection");
                false
            }
        };

        if expect_echo {
            match timeout(WAIT_FOR_CLOSE_ECHO_DURATION, stream.next()).await {
                Ok(maybe_msg) => {
                    if let Some(msg) = maybe_msg {
                        match msg {
                            Ok(msg) => match msg {
                                Message::Close(_) => {
                                    trace!("received close echo");
                                }
                                _ => {
                                    trace!("did not receive a close echo");
                                }
                            },
                            Err(e) => {
                                trace!(error = %e, "error receiving echo event");
                            }
                        }
                    } else {
                        trace!("client closed connection early");
                    }
                }
                Err(_) => {
                    trace!("did not receive echo in time");
                }
            }
        }

        info!("connection closed");
    }
}
