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
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RoomCommand {
    /// Ask for the room to be sealed - this will close the connection of all
    /// clients currently in the room, and it will prevent new clients from
    /// joining the room as well.
    SealRoom,

    /// Send a WebRTC offer to another player in the room.
    SendOffer(u32, String),

    /// Send a WebRTC answer to another player in the room.
    SendAnswer(u32, String),

    /// Send a WebRTC candidate to another player in the room.
    SendCandidate(u32, String),

    /// The client's connection should be dropped, as there is nothing else we
    /// can do with it.
    /// 
    /// **NOTE:** Unlike in [`LobbyCommand`], closing a connection is done in
    /// a separate channel, since the room needs the stream from the player task
    /// in order to close the connection properly - all of the commands in here
    /// do not need the client's stream within the room task.
    DropConnection,
}

impl fmt::Display for RoomCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SealRoom => write!(f, "seal room"),

            // DO NOT output payloads, as they can contain sensitive information!
            Self::SendOffer(player_id, _) => write!(f, "send offer to: {}", player_id),
            Self::SendAnswer(player_id, _) => write!(f, "send answer to: {}", player_id),
            Self::SendCandidate(player_id, _) => write!(f, "send candidate to: {}", player_id),

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
    /// A new player with the given ID has joined the room.
    PlayerJoined(u32),

    /// The player with the given ID has left the room.
    /// 
    /// **NOTE:** This does NOT include the host. If the host leaves the room,
    /// [`RoomNotification::HostLeft`] will be sent instead.
    PlayerLeft(u32),

    /// Received a WebRTC offer from the given player.
    OfferReceived(u32, String),

    /// Received a WebRTC answer from the given player.
    AnswerReceived(u32, String),

    /// Received a WebRTC candidate from the given player.
    CandidateReceived(u32, String),

    /// The host has left the room, closing it.
    HostLeft,

    /// The room has been sealed by the host, closing it.
    RoomSealed,

    /// An error occured, and the given close code needs to be sent out.
    Error(CloseCode),
}

impl fmt::Display for RoomNotification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoomNotification::PlayerJoined(id) => write!(f, "player joined: {}", id),
            RoomNotification::PlayerLeft(id) => write!(f, "player left: {}", id),
            RoomNotification::OfferReceived(from, _) => write!(f, "offer from: {}", from),
            RoomNotification::AnswerReceived(from, _) => write!(f, "answer from: {}", from),
            RoomNotification::CandidateReceived(from, _) => write!(f, "candidate from: {}", from),
            RoomNotification::HostLeft => write!(f, "host left"),
            RoomNotification::RoomSealed => write!(f, "room sealed"),
            RoomNotification::Error(code) => write!(f, "error (close: {})", code),
        }
    }
}
