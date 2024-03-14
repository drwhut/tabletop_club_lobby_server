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
//! arguments as an INI-style configuration file:
//! ```text
//! ./ttc-lobby -c config.ini
//! ```
//!
//! After the initial configuration has been loaded, every thirty seconds the
//! server will read the file again to see if the configuration has changed.
//! If it has, then the other tasks are notified and will adjust their behaviour
//! accordingly.

use tokio::sync::broadcast;
use tokio::time::Duration;

/// How often the task will check the configuration file.
const CONFIG_UPDATE_INTERVAL: Duration = Duration::from_secs(30);

/// The set of properties that can be updated at runtime.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct VariableConfig {
    /// The maximum size of incoming messages, in bytes.
    ///
    /// **NOTE:** Changing this property does not affect existing clients.
    pub max_message_size: usize,

    /// The maximum size of incoming message payloads, in bytes.
    ///
    /// **NOTE:** Changing this property does not affect existing clients.
    pub max_payload_size: usize,

    /// The maximum number of players allowed for each remote IP address.
    pub max_players_per_address: usize,

    /// The maximum number of players allowed in each room.
    pub max_players_per_room: usize,

    /// The maximum number of active rooms at any given time.
    pub max_rooms: usize,

    /// The maximum number of connected players that are not currently in a room.
    pub player_queue_capacity: usize,

    /// The maximum amount of time players have to either create or join a room,
    /// in seconds.
    pub join_room_time_limit: Duration,

    /// How often the server should ping players, in seconds.
    pub ping_interval: Duration,

    /// How long the server should wait for a player's response before assuming
    /// they have disconnected, in seconds.
    ///
    /// **NOTE:** This should be higher than [`VariableConfig::ping_interval`],
    /// so that players have enough opportunity to send a pong message.
    pub response_time_limit: Duration,

    /// The minimum amount of time a player needs to wait after they last
    /// connected to the server before they can reconnect, in seconds.
    pub reconnect_wait_limit: Duration,
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

            join_room_time_limit: Duration::from_secs(10),
            ping_interval: Duration::from_secs(10),
            response_time_limit: Duration::from_secs(30),
            reconnect_wait_limit: Duration::from_secs(5),
        }
    }
}
