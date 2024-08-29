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

pub use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// A custom set of close codes that the server can send to the players.
pub enum CustomCloseCode {
    /// A generic error occured.
    ///
    /// **NOTE:** This close code should never be used. Always try to use a more
    /// descriptive close code.
    Error,

    /// Failed to connect to the lobby server.
    ///
    /// **NOTE:** This close code should never be used. This is meant as an
    /// error code for clients, as is here only for reference.
    Unreachable,

    /// The client did not create or join a room in time.
    DidNotJoinRoom,

    /// The host disconnected from the room the player was in.
    HostDisconnected,

    /// Only the host of the room is allowed to seal it.
    OnlyHostCanSeal,

    /// The maximum number of rooms has been reached.
    TooManyRooms,

    /// Tried to do something that can only be done when not in a room.
    AlreadyInRoom,

    /// The room that was given by the client does not exist.
    RoomDoesNotExist,

    /// The room that was given by the client has been sealed.
    RoomSealed,

    /// The message that was given was incorrectly formatted.
    InvalidFormat,

    /// Tried to do something that can only be done while in a room.
    NotInRoom,

    /// An internal server error occured.
    ///
    /// **NOTE:** This close code is redundant, [`CloseCode::Error`] should be
    /// used instead.
    ServerError,

    /// The message that was given contained an invalid destination ID.
    InvalidDestination,

    /// The message that was given contained an invalid command.
    InvalidCommand,

    /// The maximum number of players within a room has been reached.
    TooManyPlayers,

    /// A binary message was received when only text messages are allowed.
    ///
    /// **NOTE:** This close code is redundant, [`CloseCode::Unsupported`]
    /// should be used instead.
    InvalidMode,

    /// Too many connections from the same IP address.
    ///
    /// **NOTE:** This code is redundant, as connection limiting should ideally
    /// be implemented via a reverse proxy.
    TooManyConnections,

    /// Established another connection too quickly after the last one.
    ///
    /// **NOTE:** This code is redundant, as rate limiting should ideally be
    /// implemented via a reverse proxy.
    ReconnectTooQuickly,

    /// The queue of players trying to create or join rooms is at capacity.
    JoinQueueFull,
}

impl From<CustomCloseCode> for u16 {
    fn from(value: CustomCloseCode) -> Self {
        match value {
            CustomCloseCode::Error => 4000,
            CustomCloseCode::Unreachable => 4001,
            CustomCloseCode::DidNotJoinRoom => 4002,
            CustomCloseCode::HostDisconnected => 4003,
            CustomCloseCode::OnlyHostCanSeal => 4004,
            CustomCloseCode::TooManyRooms => 4005,
            CustomCloseCode::AlreadyInRoom => 4006,
            CustomCloseCode::RoomDoesNotExist => 4007,
            CustomCloseCode::RoomSealed => 4008,
            CustomCloseCode::InvalidFormat => 4009,
            CustomCloseCode::NotInRoom => 4010,
            CustomCloseCode::ServerError => 4011,
            CustomCloseCode::InvalidDestination => 4012,
            CustomCloseCode::InvalidCommand => 4013,
            CustomCloseCode::TooManyPlayers => 4014,
            CustomCloseCode::InvalidMode => 4015,
            CustomCloseCode::TooManyConnections => 4016,
            CustomCloseCode::ReconnectTooQuickly => 4017,
            CustomCloseCode::JoinQueueFull => 4018,
        }
    }
}

impl TryFrom<u16> for CustomCloseCode {
    type Error = ();

    fn try_from(value: u16) -> Result<Self, <Self as TryFrom<u16>>::Error> {
        match value {
            4000 => Ok(Self::Error),
            4001 => Ok(Self::Unreachable),
            4002 => Ok(Self::DidNotJoinRoom),
            4003 => Ok(Self::HostDisconnected),
            4004 => Ok(Self::OnlyHostCanSeal),
            4005 => Ok(Self::TooManyRooms),
            4006 => Ok(Self::AlreadyInRoom),
            4007 => Ok(Self::RoomDoesNotExist),
            4008 => Ok(Self::RoomSealed),
            4009 => Ok(Self::InvalidFormat),
            4010 => Ok(Self::NotInRoom),
            4011 => Ok(Self::ServerError),
            4012 => Ok(Self::InvalidDestination),
            4013 => Ok(Self::InvalidCommand),
            4014 => Ok(Self::TooManyPlayers),
            4015 => Ok(Self::InvalidMode),
            4016 => Ok(Self::TooManyConnections),
            4017 => Ok(Self::ReconnectTooQuickly),
            4018 => Ok(Self::JoinQueueFull),

            _ => Err(()),
        }
    }
}

impl From<CustomCloseCode> for CloseCode {
    fn from(value: CustomCloseCode) -> Self {
        Self::Library(value.into())
    }
}

impl TryFrom<CloseCode> for CustomCloseCode {
    type Error = ();

    fn try_from(value: CloseCode) -> Result<Self, <Self as TryFrom<CloseCode>>::Error> {
        if let CloseCode::Library(code_u16) = value {
            CustomCloseCode::try_from(code_u16)
        } else {
            Err(())
        }
    }
}
