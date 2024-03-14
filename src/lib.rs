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

pub mod config;
pub mod connection;
pub mod message;
pub mod player;
pub mod room;

use std::path::PathBuf;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tracing::{error, info};

/// The properties that the [`Server`] requires.
pub struct ServerContext {
    /// The file path to the configuration file.
    pub config_file_path: PathBuf,

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
        let (config_sender, config_receiver) = watch::channel(default_config);

        // Spawn the task that checks the configuration file every so often, and
        // updates the server configuration if it changes.
        let update_config_handle = tokio::spawn(config::update_config_task(
            context.config_file_path,
            config_sender,
            context.shutdown_signal.resubscribe(),
        ));

        // Wait for the shutdown signal from the main thread.
        // NOTE: If the sender is dropped, that means the main thread is dead,
        // so this task will be aborted anyways.
        let _ = context.shutdown_signal.recv().await;

        // Now that we are shutting down, wait for all of the tasks that we have
        // spawned to end gracefully.
        if let Err(e) = update_config_handle.await {
            error!(error = %e, "update config task did not finish to completion");
        }

        info!("stopped");
    }
}
