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

use std::fmt;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RoomCode {
    bytes: [u8; 4],
}

impl RoomCode {
    pub fn random() -> Self {
        Self {
            bytes: [
                fastrand::u8(65..91),
                fastrand::u8(65..91),
                fastrand::u8(65..91),
                fastrand::u8(65..91),
            ],
        }
    }

    fn valid_bytes(bytes: [u8; 4]) -> bool {
        for b in bytes {
            if b < 65 || b > 90 {
                return false;
            }
        }

        true
    }

    // TODO: Add a function to check if the room code has profanity.
}

impl Default for RoomCode {
    fn default() -> Self {
        Self {
            // 'AAAA'.
            bytes: [65, 65, 65, 65],
        }
    }
}

impl fmt::Display for RoomCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", String::from(self))
    }
}

pub enum RoomCodeError {
    InvalidCharacters,
    InvalidLength,
}

impl TryFrom<u32> for RoomCode {
    type Error = RoomCodeError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        let bytes_from_int = value.to_be_bytes();
        if Self::valid_bytes(bytes_from_int) {
            Ok(RoomCode {
                bytes: bytes_from_int,
            })
        } else {
            Err(RoomCodeError::InvalidCharacters)
        }
    }
}

impl From<RoomCode> for u32 {
    fn from(value: RoomCode) -> Self {
        Self::from_be_bytes(value.bytes)
    }
}

impl TryFrom<&str> for RoomCode {
    type Error = RoomCodeError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let bytes_from_str = value.as_bytes();
        if bytes_from_str.len() != 4 {
            return Err(RoomCodeError::InvalidLength);
        }

        let mut fixed_length = [0, 0, 0, 0];
        for index in 0..4 {
            fixed_length[index] = bytes_from_str[index];
        }

        if Self::valid_bytes(fixed_length) {
            Ok(RoomCode {
                bytes: fixed_length,
            })
        } else {
            Err(RoomCodeError::InvalidCharacters)
        }
    }
}

impl TryFrom<String> for RoomCode {
    type Error = RoomCodeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.as_ref())
    }
}

impl From<&RoomCode> for String {
    fn from(value: &RoomCode) -> Self {
        String::from_utf8_lossy(&value.bytes).into_owned()
    }
}

impl From<RoomCode> for String {
    fn from(value: RoomCode) -> Self {
        String::from_utf8_lossy(&value.bytes).into_owned()
    }
}
