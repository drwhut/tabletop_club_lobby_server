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

/// A human-readable unique identifier for rooms within the lobby.
/// 
/// The current format is four ASCII characters, from 'A' to 'Z'.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RoomCode {
    bytes: [u8; 4],
}

impl RoomCode {
    /// Generate a random, valid room code.
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

    /// Check if the given sequence of `bytes` would make a valid room code.
    fn valid_bytes(bytes: [u8; 4]) -> bool {
        for b in bytes {
            if b < 65 || b > 90 {
                return false;
            }
        }

        true
    }

    /// Check if the current room code is valid.
    /// 
    /// **NOTE:** This function is for testing purposes only.
    #[cfg(test)]
    pub fn is_valid(&self) -> bool {
        Self::valid_bytes(self.bytes)
    }

    /// Check if the room code is considered profanity.
    /// 
    /// This should be used to make sure that lobbies are not given lobby codes
    /// with profanity, as we don't want to traumatise the children.
    pub fn is_profanity(&self) -> bool {
        // The full list of room codes that are considered profanity are in
        // `profanity.txt` - we're assuming the list is in order so that we can
        // use binary search here.
        let mut left: isize = 0;
        let mut right: isize = (Self::get_profanity_len() - 1) as isize;
        while left <= right {
            let middle = (left + right) / 2;

            let cmp = Self::compare_to_profanity(self, middle as usize);
            if cmp < 0 {
                right = middle - 1;
            } else if cmp > 0 {
                left = middle + 1;
            } else {
                return true;
            }
        }

        false
    }

    /// Get the profanity room code at the given `index`.
    fn get_profanity_at(index: usize) -> &'static [u8] {
        let start = 5 * index;
        &include_bytes!("profanity.txt")[start..start + 4]
    }

    /// Get the total number of lobby codes that are considered profanity.
    fn get_profanity_len() -> usize {
        include_bytes!("profanity.txt").len() / 5
    }

    /// Compare the given lobby code to the profanity at the given `index`.
    /// Returns `-` if less than, `0` if equal, and `1` if greater than.
    fn compare_to_profanity(code: &RoomCode, index: usize) -> i8 {
        let profanity = Self::get_profanity_at(index);
        for index in 0..4 {
            if code.bytes[index] < profanity[index] {
                return -1;
            } else if code.bytes[index] > profanity[index] {
                return 1;
            }
        }

        0
    }
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

/// A list of reasons why a room code might be invalid.
#[derive(Debug)]
pub enum RoomCodeError {
    /// The room code contains invalid characters.
    InvalidCharacters,

    /// The room code is the wrong length.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_default() {
        assert!(RoomCode::default().is_valid());
    }

    #[test]
    fn check_random() {
        for _ in 0..10_000_000 {
            assert!(RoomCode::random().is_valid());
        }
    }

    #[test]
    fn check_profanity() {
        let mut profanity_count = 0;
        for c1 in 65..91 {
            for c2 in 65..91 {
                for c3 in 65..91 {
                    for c4 in 65..91 {
                        let code_int = u32::from_be_bytes([c1, c2, c3, c4]);
                        let room_code = RoomCode::try_from(code_int)
                                .expect("room code considered invalid");
                        
                        if room_code.is_profanity() {
                            profanity_count += 1;
                        }
                    }
                }
            }
        }

        assert_eq!(profanity_count, 160);
    }

    /// A helper macro for checking conversions to and from integers and strings.
    macro_rules! str_and_u32_test {
        ($s:literal, $u:literal, $v:literal) => {
            // Check the string and the u32 values are equivalent.
            assert_eq!($s.len(), 4);

            let mut chars = $s.chars();
            let mut str_as_u32: u32 = 0;
            for index in 0..4 {
                let char_as_u32 = chars.next().unwrap() as u32;
                str_as_u32 |= (char_as_u32 << (8 * (3 - index)));
            }
            assert_eq!(str_as_u32, $u);

            // Convert to and from u32.
            let res = RoomCode::try_from($u as u32);
            if ($v) {
                let code = res.expect("valid u32 considered invalid");
                let same: u32 = code.into();
                assert_eq!(same, $u as u32);
            } else {
                res.expect_err("invalid u32 considered valid");
            }

            // Convert from &str.
            let res = RoomCode::try_from($s as &str);
            if ($v) {
                res.expect("valid &str considered invalid");
            } else {
                res.expect_err("invalid &str considered valid");
            }

            // Convert to and from String.
            let full_str = String::from($s);
            let res = RoomCode::try_from(full_str.clone());
            if ($v) {
                let code = res.expect("valid string considered invalid");
                let same: String = code.into();
                assert_eq!(same, full_str);
            } else {
                res.expect_err("invalid string considered valid");
            }
        };
    }

    #[test]
    fn check_conversion() {
        // Valid room codes.
        str_and_u32_test!("AAAA", 0x41414141, true);
        str_and_u32_test!("ABCD", 0x41424344, true);
        str_and_u32_test!("PLZZ", 0x504c5a5a, true);
        str_and_u32_test!("DNTE", 0x444e5445, true);
        str_and_u32_test!("TOMY", 0x544f4d59, true);
        str_and_u32_test!("KOFI", 0x4b4f4649, true);
        str_and_u32_test!("ZZZZ", 0x5a5a5a5a, true);

        // Lowercase characters.
        str_and_u32_test!("aaaa", 0x61616161, false);
        str_and_u32_test!("AAAa", 0x41414161, false);
        str_and_u32_test!("KoFi", 0x4b6f4669, false);
        str_and_u32_test!("ISdr", 0x49536472, false);
        str_and_u32_test!("whut", 0x77687574, false);
        str_and_u32_test!("dOOT", 0x644f4f54, false);

        // Numbers.
        str_and_u32_test!("0000", 0x30303030, false);
        str_and_u32_test!("1234", 0x31323334, false);
        str_and_u32_test!("1JKL", 0x314a4b4c, false);
        str_and_u32_test!("5678", 0x35363738, false);
        str_and_u32_test!("9999", 0x39393939, false);

        // Other ASCII characters.
        str_and_u32_test!("NEW\n", 0x4e45570a, false);
        str_and_u32_test!("ABC ", 0x41424320, false);
        str_and_u32_test!("BOB!", 0x424f4221, false);
        str_and_u32_test!("\"QUA", 0x22515541, false);
        str_and_u32_test!("#LOL", 0x234c4f4c, false);
        str_and_u32_test!("AUS$", 0x41555324, false);
        str_and_u32_test!("%%%%", 0x25252525, false);
        str_and_u32_test!("N&SW", 0x4e265357, false);
        str_and_u32_test!("WA'A", 0x57412741, false);
        str_and_u32_test!("(HI)", 0x28484929, false);
        str_and_u32_test!("F***", 0x462a2a2a, false);
        str_and_u32_test!("A+B=", 0x412b423d, false);
        str_and_u32_test!("Q,G.", 0x512c472e, false);
        str_and_u32_test!("/SAY", 0x2f534159, false);
        str_and_u32_test!("VAR:", 0x5641523a, false);
        str_and_u32_test!("CMD;", 0x434d443b, false);
        str_and_u32_test!("<ME>", 0x3c4d453e, false);
        str_and_u32_test!("WHA?", 0x5748413f, false);
        str_and_u32_test!("@.@ ", 0x402e4020, false);
        str_and_u32_test!("[WO]", 0x5b574f5d, false);
        str_and_u32_test!("\\^UP", 0x5c5e5550, false);
        str_and_u32_test!("_``_", 0x5f60605f, false);
        str_and_u32_test!("{|~}", 0x7b7c7e7d, false);
    }
}
