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
use std::fmt;
use std::ops::Range;
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
            max_message_size: 10100,
            max_payload_size: 10000,

            max_players_per_address: 5,
            max_players_per_room: 10,
            max_rooms: 100,
            player_queue_capacity: 100,

            join_room_time_limit_secs: 5,
            ping_interval_secs: 10,
            response_time_limit_secs: 30,
            reconnect_wait_limit_secs: 5,
        }
    }
}

/// Helper macro for checking if the value of a property in the config is within
/// a certain range, and if it isn't, it returns a [`VariableConfigError`].
macro_rules! check_range {
    ($o:ident, $k:ident, $r:expr) => {
        if !($r).contains(&$o.$k) {
            return Err(VariableConfigError::OutOfRange{
                key: stringify!($k),
                value: $o.$k as usize,
                range: $r
            })
        }
    }
}

/// Helper macro for checking if the value of a property is less than the value
/// of another, and if it isn't, it returns a [`VariableConfigError`].
macro_rules! check_lt {
    ($o:ident, $l:ident, $g:ident) => {
        if ($o.$l) >= ($o.$g) {
            return Err(VariableConfigError::GreaterOrEqual{
                key1: stringify!($l),
                value1: $o.$l as usize,
                key2: stringify!($g),
                value2: $o.$g as usize
            })
        }
    };
}

impl VariableConfig {
    /// Read the configuration file at the given path.
    pub async fn read_config_file(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let file_content = fs::read_to_string(path).await?;

        match toml_edit::de::from_str::<VariableConfig>(&file_content) {
            Ok(config) => match config.validate() {
                Ok(_) => Ok(config),
                Err(e) => Err(std::io::Error::other(e))
            },
            Err(e) => Err(std::io::Error::other(e))
        }
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
# NOTE: Changing this property does not affect existing clients.
# NOTE: This must be smaller than 'max_message_size', as the message contains
# the payload.\n",
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

    /// Check if the values assigned to the current config are valid.
    pub fn validate(&self) -> Result<(), VariableConfigError> {
        check_range!(self, max_message_size, 100..100_000_000); // 100MB.
        check_range!(self, max_payload_size, 100..100_000_000);
        check_range!(self, max_players_per_address, 1..1000);
        check_range!(self, max_players_per_room, 1..100);
        check_range!(self, max_rooms, 1..400_000);
        check_range!(self, player_queue_capacity, 1..10_000);
        check_range!(self, join_room_time_limit_secs, 1..60);
        check_range!(self, ping_interval_secs, 1..60);
        check_range!(self, response_time_limit_secs, 5..120);
        check_range!(self, reconnect_wait_limit_secs, 0..60);

        check_lt!(self, max_payload_size, max_message_size);
        check_lt!(self, join_room_time_limit_secs, ping_interval_secs);
        check_lt!(self, ping_interval_secs, response_time_limit_secs);

        Ok(())
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

/// The types of validation errors that can occur when reading the config file.
/// 
/// **NOTE:** This does not include invalid types, or filesystem errors. These
/// are handled by the libraries.
#[derive(Debug)]
pub enum VariableConfigError {
    OutOfRange{ key: &'static str, value: usize, range: Range<usize> },
    GreaterOrEqual{ key1: &'static str, value1: usize, key2: &'static str, value2: usize },
}

impl fmt::Display for VariableConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange { key, value, range } => write!(f, "value of `{}` is out of range (range: {}-{}, got: {})",
                    key, range.start, range.end - 1, value),
            Self::GreaterOrEqual { key1, value1, key2, value2 } =>
                    write!(f, "value of `{}` ({}) is more than or equal to the value of `{}` ({})",
                            key1, value1, key2, value2),
        }
    }
}

impl std::error::Error for VariableConfigError {}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_default() {
        assert!(VariableConfig::default().validate().is_ok())
    }

    #[tokio::test]
    async fn read_and_write() -> Result<(), std::io::Error> {
        let mut config = VariableConfig::default();
        config.max_players_per_address = 15;
        config.max_rooms = 250;
        config.player_queue_capacity = 10;
        config.ping_interval_secs = 6;

        config.write_config_file("__read_and_write__.toml").await?;

        let config_read = VariableConfig::read_config_file("__read_and_write__.toml").await?;
        assert_eq!(config_read, config);

        fs::remove_file("__read_and_write__.toml").await?;

        Ok(())
    }

    /// Helper macro for testing if, when a section of a valid config file is
    /// replaced, whether it results in an error with the given text.
    macro_rules! check_replace {
        ($b:literal, $a:literal, $e:literal) => {
            let content = String::from(VALID_CONTENTS).replace($b, $a);
            fs::write("__read_err__.toml", content).await?;
            let e = VariableConfig::read_config_file("__read_err__.toml").await.unwrap_err();
            assert_eq!(e.to_string(), $e);
        };
    }

    /// The contents of a valid config file.
    const VALID_CONTENTS: &str = "
max_message_size = 1100
max_payload_size = 1000
max_players_per_address = 10
max_players_per_room = 10
max_rooms = 10
player_queue_capacity = 10
join_room_time_limit_secs = 3
ping_interval_secs = 5
response_time_limit_secs = 20
reconnect_wait_limit_secs = 5";

    #[tokio::test]
    async fn read_err() -> Result<(), std::io::Error> {
        // File with no values.
        fs::write("__read_err__.toml", "").await?;
        let e = VariableConfig::read_config_file("__read_err__.toml").await.unwrap_err();
        assert_eq!(e.to_string(), "TOML parse error at line 1, column 1\n  |\n1 | \n  | ^\nmissing field `max_message_size`\n");

        // File with one value missing.
        check_replace!("max_rooms = 10", "",
                "TOML parse error at line 1, column 1\n  |\n1 | \n  | ^\nmissing field `max_rooms`\n");

        // File with all values (checking VALID_CONTENTS is actually valid).
        fs::write("__read_err__.toml", VALID_CONTENTS).await?;
        assert!(VariableConfig::read_config_file("__read_err__.toml").await.is_ok());

        // Value with wrong type.
        check_replace!("player_queue_capacity = 10", "player_queue_capacity = \"10\"",
                "TOML parse error at line 7, column 25\n  |\n7 | player_queue_capacity = \"10\"\n  |                         ^^^^\ninvalid type: string \"10\", expected usize\n");

        // Negative value.
        check_replace!("max_players_per_room = 10", "max_players_per_room = -1",
                "TOML parse error at line 5, column 24\n  |\n5 | max_players_per_room = -1\n  |                        ^^\ninvalid value: integer `-1`, expected usize\n");

        // Out of range values.
        check_replace!("max_message_size = 1100", "max_message_size = 10",
                "value of `max_message_size` is out of range (range: 100-99999999, got: 10)");
        check_replace!("max_message_size = 1100", "max_message_size = 500000000",
                "value of `max_message_size` is out of range (range: 100-99999999, got: 500000000)");

        check_replace!("max_payload_size = 1000", "max_payload_size = 99",
                "value of `max_payload_size` is out of range (range: 100-99999999, got: 99)");
        check_replace!("max_payload_size = 1000", "max_payload_size = 250000000",
                "value of `max_payload_size` is out of range (range: 100-99999999, got: 250000000)");

        check_replace!("max_players_per_address = 10", "max_players_per_address = 0",
                "value of `max_players_per_address` is out of range (range: 1-999, got: 0)");
        check_replace!("max_players_per_address = 10", "max_players_per_address = 1000",
                "value of `max_players_per_address` is out of range (range: 1-999, got: 1000)");

        check_replace!("max_players_per_room = 10", "max_players_per_room = 0",
                "value of `max_players_per_room` is out of range (range: 1-99, got: 0)");
        check_replace!("max_players_per_room = 10", "max_players_per_room = 150",
                "value of `max_players_per_room` is out of range (range: 1-99, got: 150)");

        check_replace!("max_rooms = 10", "max_rooms = 0",
                "value of `max_rooms` is out of range (range: 1-399999, got: 0)");
        check_replace!("max_rooms = 10", "max_rooms = 500000",
                "value of `max_rooms` is out of range (range: 1-399999, got: 500000)");
        
        check_replace!("player_queue_capacity = 10", "player_queue_capacity = 0",
                "value of `player_queue_capacity` is out of range (range: 1-9999, got: 0)");
        check_replace!("player_queue_capacity = 10", "player_queue_capacity = 50000",
                "value of `player_queue_capacity` is out of range (range: 1-9999, got: 50000)");

        check_replace!("join_room_time_limit_secs = 3", "join_room_time_limit_secs = 0",
                "value of `join_room_time_limit_secs` is out of range (range: 1-59, got: 0)");
        check_replace!("join_room_time_limit_secs = 3", "join_room_time_limit_secs = 120",
                "value of `join_room_time_limit_secs` is out of range (range: 1-59, got: 120)");
        
        check_replace!("ping_interval_secs = 5", "ping_interval_secs = 0",
                "value of `ping_interval_secs` is out of range (range: 1-59, got: 0)");
        check_replace!("ping_interval_secs = 5", "ping_interval_secs = 90",
                "value of `ping_interval_secs` is out of range (range: 1-59, got: 90)");
        
        check_replace!("response_time_limit_secs = 20", "response_time_limit_secs = 4",
                "value of `response_time_limit_secs` is out of range (range: 5-119, got: 4)");
        check_replace!("response_time_limit_secs = 20", "response_time_limit_secs = 150",
                "value of `response_time_limit_secs` is out of range (range: 5-119, got: 150)");
        
        // No lower bound for 'reconnect_wait_limit_secs'.
        check_replace!("reconnect_wait_limit_secs = 5", "reconnect_wait_limit_secs = 120",
                "value of `reconnect_wait_limit_secs` is out of range (range: 0-59, got: 120)");
        
        // Certain properties cannot be greater than others.
        check_replace!("max_message_size = 1100", "max_message_size = 900",
                "value of `max_payload_size` (1000) is more than or equal to the value of `max_message_size` (900)");
        check_replace!("max_payload_size = 1000", "max_payload_size = 1100",
                "value of `max_payload_size` (1100) is more than or equal to the value of `max_message_size` (1100)");

        check_replace!("join_room_time_limit_secs = 3", "join_room_time_limit_secs = 6",
                "value of `join_room_time_limit_secs` (6) is more than or equal to the value of `ping_interval_secs` (5)");
        check_replace!("ping_interval_secs = 5", "ping_interval_secs = 2",
                "value of `join_room_time_limit_secs` (3) is more than or equal to the value of `ping_interval_secs` (2)");
        
        check_replace!("ping_interval_secs = 5", "ping_interval_secs = 30",
                "value of `ping_interval_secs` (30) is more than or equal to the value of `response_time_limit_secs` (20)");
        check_replace!("response_time_limit_secs = 20", "response_time_limit_secs = 5",
                "value of `ping_interval_secs` (5) is more than or equal to the value of `response_time_limit_secs` (5)");

        fs::remove_file("__read_err__.toml").await?;

        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn update_task_no_file() {
        let (watch_send, mut watch_receive) = watch::channel(VariableConfig::default());
        let (shutdown_send, shutdown_receive) = broadcast::channel::<()>(1);

        // Verify that the file doesn't exist.
        let test_file_exists = fs::try_exists("__no_file__.toml").await.expect("could not check if file exists");
        assert!(!test_file_exists);

        // If the file doesn't exist, then no updates should come through the
        // watch channel immediately.
        let handle = tokio::spawn(update_config_task("__no_file__.toml",
                watch_send, shutdown_receive));
        
        assert_eq!(*watch_receive.borrow(), VariableConfig::default());
        assert!(!watch_receive.has_changed().unwrap());

        shutdown_send.send(()).expect("failed to send shutdown signal");
        handle.await.expect("task was aborted");
    }

    #[tokio::test(start_paused = true)]
    async fn update_task_default_file() {
        let (watch_send, mut watch_receive) = watch::channel(VariableConfig::default());
        let (shutdown_send, shutdown_receive) = broadcast::channel::<()>(1);

        // Create a config file with the default settings.
        VariableConfig::default().write_config_file("__default__.toml").await.expect("failed to write config file");

        // If a config file exists, but it has the default settings, then no
        // updates should come through the watch channel immediately.
        let handle = tokio::spawn(update_config_task("__default__.toml",
                watch_send, shutdown_receive));

        assert_eq!(*watch_receive.borrow(), VariableConfig::default());
        assert!(!watch_receive.has_changed().unwrap());

        // Make a new config, and save it over the old file.
        let test_config = VariableConfig {
            max_message_size: 600,
            max_payload_size: 500,
            max_players_per_address: 1,
            max_players_per_room: 5,
            max_rooms: 10,
            player_queue_capacity: 5,
            join_room_time_limit_secs: 5,
            ping_interval_secs: 10,
            response_time_limit_secs: 20,
            reconnect_wait_limit_secs: 5
        };
        assert!(test_config.validate().is_ok());
        
        test_config.write_config_file("__default__.toml").await.expect("failed to write config file");

        // Now the config file has updated, the task's next pass should detect
        // the change and send an update to the watch channel.
        tokio::time::advance(CONFIG_UPDATE_INTERVAL).await;

        watch_receive.changed().await.unwrap();
        assert_eq!(*watch_receive.borrow(), test_config);

        shutdown_send.send(()).expect("failed to send shutdown signal");
        handle.await.expect("task was aborted");

        fs::remove_file("__default__.toml").await.expect("failed to remove test file");
    }

    #[tokio::test(start_paused = true)]
    async fn update_task_modified_file() {
        let (watch_send, mut watch_receive) = watch::channel(VariableConfig::default());
        let (shutdown_send, shutdown_receive) = broadcast::channel::<()>(1);

        // Make a modified config, and save it to a test file.
        let test_config = VariableConfig {
            max_message_size: 200,
            max_payload_size: 100,
            max_players_per_address: 100,
            max_players_per_room: 50,
            max_rooms: 100,
            player_queue_capacity: 500,
            join_room_time_limit_secs: 30,
            ping_interval_secs: 40,
            response_time_limit_secs: 60,
            reconnect_wait_limit_secs: 10
        };
        assert!(test_config.validate().is_ok());

        test_config.write_config_file("__modified__.toml").await.expect("failed to write config file");

        // If a config file exists, and it differs from the default, then the
        // task should immediately send an update through the watch channel.
        let handle = tokio::spawn(update_config_task("__modified__.toml",
                watch_send, shutdown_receive));

        watch_receive.changed().await.unwrap();
        assert_eq!(*watch_receive.borrow(), test_config);

        // Make a new config, and save it over the old file.
        let test_config = VariableConfig {
            max_message_size: 3000,
            max_payload_size: 2500,
            max_players_per_address: 20,
            max_players_per_room: 10,
            max_rooms: 1000,
            player_queue_capacity: 100,
            join_room_time_limit_secs: 10,
            ping_interval_secs: 15,
            response_time_limit_secs: 20,
            reconnect_wait_limit_secs: 5
        };
        assert!(test_config.validate().is_ok());
        
        test_config.write_config_file("__modified__.toml").await.expect("failed to write config file");

        // Now the config file has updated, the task's next pass should detect
        // the change and send another update to the watch channel.
        tokio::time::advance(CONFIG_UPDATE_INTERVAL).await;

        watch_receive.changed().await.unwrap();
        assert_eq!(*watch_receive.borrow(), test_config);

        shutdown_send.send(()).expect("failed to send shutdown signal");
        handle.await.expect("task was aborted");

        fs::remove_file("__modified__.toml").await.expect("failed to remove test file");
    }
}
