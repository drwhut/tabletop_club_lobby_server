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

//! A series of message types to be sent between the lobby, rooms, and clients.

use crate::close_code::CloseCode;
use crate::room_code::RoomCode;

use std::fmt;

/// A command from an incoming client to the lobby.
#[derive(Debug, Eq, PartialEq)]
pub enum LobbyCommand {
    /// The client wishes to create a new room, with itself as the host.
    CreateRoom,

    /// The client wishes to join an existing room, with the given room code.
    JoinRoom(RoomCode),

    /// The client's connection should be closed gracefully with the given close
    /// code.
    CloseConnection(CloseCode),

    /// The client's connection should be dropped, as there is nothing else to
    /// do.
    DropConnection,
}

impl fmt::Display for LobbyCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateRoom => write!(f, "create room"),
            Self::JoinRoom(code) => write!(f, "join room {}", code),
            Self::CloseConnection(code) => write!(f, "close connection (code: {})", code),
            Self::DropConnection => write!(f, "drop connection"),
        }
    }
}

/// A request from a client (with the given `handle_id`) to the lobby.
#[derive(Debug)]
pub struct LobbyRequest {
    pub handle_id: u32,
    pub command: LobbyCommand,
}

/// A command from a client to the room it has joined.
#[derive(Debug, Eq, PartialEq)]
pub enum RoomCommand {
    /// The client's connection should be dropped, as there is nothing else we
    /// can do with it.
    DropConnection,
}

impl fmt::Display for RoomCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DropConnection => write!(f, "drop connection"),
        }
    }
}

/// A request from a player (with the given `player_id`) to the room the player
/// has joined.
#[derive(Debug)]
pub struct RoomRequest {
    pub player_id: u32,
    pub command: RoomCommand,
}

/// A notification from a room, to any given player.
#[derive(Debug)]
pub enum RoomNotification {
    
}
