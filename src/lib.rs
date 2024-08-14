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

pub mod close_code;
pub mod config;
pub mod connection;
pub mod lobby;
pub mod message;
pub mod player;
pub mod room;

use std::path::PathBuf;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_native_tls::native_tls::Identity;
use tokio_util::task::TaskTracker;
use tracing::{error, info, warn};

/// The number of connections and their remote addresses that can be in the
/// buffer on the way to the lobby task until senders need to wait for space.
const LOBBY_CONNECTION_CHANNEL_BUFFER_SIZE: usize = 20;

/// The properties that the [`Server`] requires.
pub struct ServerContext {
    /// The file path to the configuration file.
    pub config_file_path: PathBuf,

    /// The [`TcpListener`] to listen to connections from.
    pub tcp_listener: TcpListener,

    /// The cryptographic identity of the server.
    pub tls_identity: Option<Identity>,

    /// The shutdown signal from the main thread.
    pub shutdown_signal: broadcast::Receiver<()>,
}

/// The wrapper around the lobby server task.
pub struct Server {
    handle: JoinHandle<()>,
}

impl Server {
    /// Spawn the server task with the given `context`.
    pub fn spawn(context: ServerContext) -> Self {
        Self {
            handle: tokio::spawn(Self::task(context)),
        }
    }

    /// Get the [`JoinHandle`] for the server task.
    pub fn handle(&mut self) -> &mut JoinHandle<()> {
        &mut self.handle
    }

    /// The main server task, which runs in it's own thread.
    #[tracing::instrument(name = "server", skip_all)]
    async fn task(mut context: ServerContext) {
        info!("initialising");

        // Get the default configuration.
        let default_config = config::VariableConfig::default();

        // Create a watch channel so that the various tasks can keep track of
        // changes to the configuration.
        let (config_sender, mut config_receiver) = watch::channel(default_config);

        // Spawn the task that checks the configuration file every so often, and
        // updates the server configuration if it changes.
        let update_config_handle = tokio::spawn(config::update_config_task(
            context.config_file_path,
            config_sender,
            context.shutdown_signal.resubscribe(),
        ));

        // Create a channel for sending WebSocket connections and their remote
        // addresses to the lobby task.
        let (conn_sender, conn_receiver) = mpsc::channel(LOBBY_CONNECTION_CHANNEL_BUFFER_SIZE);

        // Create the lobby task, and send it the data it needs to function.
        let lobby_context = lobby::LobbyContext {
            connection_receiver: conn_receiver,
            config_receiver: config_receiver.clone(),
            shutdown_signal: context.shutdown_signal.resubscribe(),
        };

        // Spawn the lobby task.
        let mut lobby = lobby::Lobby::spawn(lobby_context);

        // Keep track of what the configuration says the maximum message and
        // payload sizes should be.
        let mut max_message_size = default_config.max_message_size;
        let mut max_payload_size = default_config.max_payload_size;

        // Since we spawn tasks for each connection that we accept, we need to
        // track them in the event that we receive the shutdown signal, and need
        // to wait for them to all finish before we can exit.
        // NOTE: We're using tokio_util::task::TaskTracker instead of
        // tokio::task::JoinSet since this frees memory once a task is complete.
        let conn_task_tracker = TaskTracker::new();

        match context.tcp_listener.local_addr() {
            Ok(addr) => {
                info!(addr = %addr, "listening for connections");
            }
            Err(e) => {
                warn!(error = %e, "could not get listener address");
            }
        }

        // Wait for connections until we receive the shutdown signal.
        loop {
            tokio::select! {
                res = context.tcp_listener.accept() => {
                    match res {
                        Ok((tcp_stream, remote_addr)) => {
                            // Attempt to accept the connection in a separate
                            // task, and keep track of the task in case we need
                            // to shut down in the middle of it.
                            let conn_context = connection::ConnectionContext {
                                tcp_stream: tcp_stream,
                                tls_identity: context.tls_identity.clone(),
                                remote_addr: remote_addr,
                                max_message_size: max_message_size,
                                max_payload_size: max_payload_size,
                                send_to_lobby: conn_sender.clone(),
                                shutdown_signal: context.shutdown_signal.resubscribe(),
                            };

                            conn_task_tracker.spawn(connection::accept_connection(conn_context));
                        },
                        Err(e) => {
                            error!(error = %e, "failed to accept connection");
                        }
                    }
                },

                // Watch for changes to the configuration - if the sender has
                // been dropped then we'll keep the config we have.
                Ok(()) = config_receiver.changed() => {
                    let new_config = config_receiver.borrow_and_update();

                    max_message_size = new_config.max_message_size;
                    max_payload_size = new_config.max_payload_size;
                    info!(max_message_size, max_payload_size, "stream config updated");
                },

                // If the sender gets dropped, then we want to exit anyway.
                _ = context.shutdown_signal.recv() => {
                    break;
                }
            }
        }

        // Now that we are shutting down, wait for all of the tasks that we have
        // spawned to end gracefully.
        if !conn_task_tracker.is_empty() {
            info!(
                num_tasks = conn_task_tracker.len(),
                "waiting for accept tasks to finish"
            );
        }
        conn_task_tracker.close();
        conn_task_tracker.wait().await;

        if let Err(e) = lobby.handle().await {
            error!(error = %e, "lobby task did not finish to completion");
        }

        if let Err(e) = update_config_handle.await {
            error!(error = %e, "update config task did not finish to completion");
        }

        info!("stopped");
    }
}
