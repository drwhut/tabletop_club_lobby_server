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

//! Read and check for updates to the configuration file.
//!
//! When the server starts, it will read the file given in the command line
//! arguments as an TOML-style configuration file:
//! ```text
//! ./ttc-lobby -c server.toml
//! ```
//!
//! After the initial configuration has been loaded, every thirty seconds the
//! server will read the file again to see if the configuration has changed.
//! If it has, then the other tasks are notified and will adjust their behaviour
//! accordingly.

use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::fs;
use tokio::sync::{broadcast, watch};
use tokio::time::{interval, Duration};
use toml_edit::DocumentMut;
use tracing::{error, info, trace, warn};

/// How often the task will check the configuration file.
const CONFIG_UPDATE_INTERVAL: Duration = Duration::from_secs(30);

/// The set of properties that can be updated at runtime.
#[derive(Debug, Deserialize, Copy, Clone, Eq, PartialEq, Serialize)]
pub struct VariableConfig {
    pub max_message_size: usize,
    pub max_payload_size: usize,

    pub max_players_per_address: usize,
    pub max_players_per_room: usize,
    pub max_rooms: usize,
    pub player_queue_capacity: usize,

    pub join_room_time_limit_secs: u64,
    pub ping_interval_secs: u64,
    pub response_time_limit_secs: u64,
    pub reconnect_wait_limit_secs: u64,
}

impl Default for VariableConfig {
    fn default() -> Self {
        Self {
            // 10KB should be enough for the offers, answers, and candidates.
            max_message_size: 10000,
            max_payload_size: 10000,

            max_players_per_address: 5,
            max_players_per_room: 10,
            max_rooms: 100,
            player_queue_capacity: 100,

            join_room_time_limit_secs: 10,
            ping_interval_secs: 10,
            response_time_limit_secs: 30,
            reconnect_wait_limit_secs: 5,
        }
    }
}

impl VariableConfig {
    /// Read the configuration file at the given path.
    pub async fn read_config_file(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let file_content = fs::read_to_string(path).await?;
        toml_edit::de::from_str(&file_content).map_err(|e| std::io::Error::other(e))
    }

    /// Write this configuration to the file at the given path with comments.
    pub async fn write_config_file(&self, path: impl AsRef<Path>) -> Result<(), std::io::Error> {
        let mut document =
            toml_edit::ser::to_document(&self).map_err(|e| std::io::Error::other(e))?;

        Self::set_key_prefix(
            &mut document,
            "max_message_size",
            "# Tabletop Club Lobby Server - Configuration File
#
# This file is used to configure the lobby server, even while it is running.
# To use this configuration file, pass it as an argument when running the server:
#
#    ./ttc-lobby -c server.toml
#
# The server will then periodically check the given file path for updates to the
# configuration. Please wait up to thirty seconds for the changes to take effect.

# The maximum length of client messages in bytes.
# NOTE: Changing this property does not affect existing clients.\n",
        );

        Self::set_key_prefix(
            &mut document,
            "max_payload_size",
            "
# The maximum length of client message payloads in bytes.
# NOTE: Changing this property does not affect existing clients.\n",
        );

        Self::set_key_prefix(
            &mut document,
            "max_players_per_address",
            "
# The maximum number of clients allowed per remote address.\n",
        );

        Self::set_key_prefix(
            &mut document,
            "max_players_per_room",
            "
# The maximum number of players allowed in each room.\n",
        );

        Self::set_key_prefix(
            &mut document,
            "max_rooms",
            "
# The maximum number of active rooms at any given time.
# NOTE: This property, along with 'max_players_per_room', determines the maximum
# number of active players.\n",
        );

        Self::set_key_prefix(
            &mut document,
            "player_queue_capacity",
            "
# The maximum number of players that can be waiting to create or join a room.\n",
        );

        Self::set_key_prefix(
            &mut document,
            "join_room_time_limit_secs",
            "
# How long the server will give players to create or join a room, in seconds.
# NOTE: The time limit should be relatively short, as no ping messages are sent
# by the server while it is waiting for the player's request.\n",
        );

        Self::set_key_prefix(
            &mut document,
            "ping_interval_secs",
            "
# How often the server will send ping packets to players, in seconds.\n",
        );

        Self::set_key_prefix(
            &mut document,
            "response_time_limit_secs",
            "
# How long the server will wait for a player's response before it times out, in seconds.
# NOTE: This must be longer than 'ping_interval_secs', as the players need
# enough time to send pong packets.\n",
        );

        Self::set_key_prefix(
            &mut document,
            "reconnect_wait_limit_secs",
            "
# How long players need to wait after their previous connection before being
# able to connect again, in seconds.\n",
        );

        fs::write(path, document.to_string().as_bytes()).await
    }

    /// Set the given key's prefix in the TOML document.
    ///
    /// If the key does not exist, a warning is shown.
    fn set_key_prefix(document: &mut DocumentMut, key: &str, prefix: &str) {
        match document.key_mut(key) {
            Some(mut key_data) => {
                key_data.leaf_decor_mut().set_prefix(prefix);
            }
            None => {
                warn!(key, "key not found in config");
            }
        }
    }
}

/// A task which periodically reads the configuration file, and if it has been
/// updated, will set the watch send value to the new configuration.
///
/// If the file cannot be read, or there was an error getting the properties
/// from the file, then an error is logged in each iteration.
#[tracing::instrument(name = "update_config", skip_all)]
pub async fn update_config_task(
    config_file_path: impl AsRef<Path> + Clone,
    config_watch: watch::Sender<VariableConfig>,
    mut shutdown_signal: broadcast::Receiver<()>,
) {
    info!("task started");

    // TODO: Find a more efficient way to check if the file has been updated -
    // maybe something like inotify, but async-safe?
    let mut interval = interval(CONFIG_UPDATE_INTERVAL);

    loop {
        tokio::select! {
            // NOTE: The first tick will occur immediately.
            _ = interval.tick() => {
                match VariableConfig::read_config_file(config_file_path.clone()).await {
                    Ok(config) => {
                        let modified = config_watch.send_if_modified(|cfg: &mut VariableConfig| {
                            if config != *cfg {
                                *cfg = config;
                                return true;
                            }
                            false
                        });

                        if modified {
                            info!("config updated");
                        } else {
                            trace!("config unchanged");
                        }
                    },
                    Err(e) => {error!(error = %e, "failed to read config file");}
                }
            },
            _ = shutdown_signal.recv() => {
                // We have been told to shut down, break out of the loop.
                break;
            }
        }
    }

    info!("task stopped");
}
