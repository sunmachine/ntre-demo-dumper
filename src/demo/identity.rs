//! A demo's identity comes from its bytes, not its file name, so the same
//! recording gets the same id in every database and a second parse of it,
//! under any name, can be recognised.

use sha2::{Digest, Sha256};

/// Hex digits of the hash that make up the id.
const ID_HEX_DIGITS: usize = 11;

pub struct DemoIdentity {
    /// SHA-256 of the whole file in lower-case hex, as `sha256sum` prints it.
    pub sha256: String,
    /// The first 11 hex digits of `sha256` read as an integer, which is 44
    /// bits. Every consumer that stores numbers as doubles, such as
    /// JavaScript and spreadsheets, keeps an integer that size exact. Two
    /// demos that share an id fail the `demos` primary key on insert, so a
    /// collision is caught rather than lost.
    pub id: i64,
}

impl DemoIdentity {
    pub fn of(data: &[u8]) -> Self {
        let sha256 = format!("{:x}", Sha256::digest(data));
        let id = i64::from_str_radix(&sha256[..ID_HEX_DIGITS], 16).expect("sha256 is hex");
        Self { sha256, id }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answer_for_abc() {
        let identity = DemoIdentity::of(b"abc");
        assert_eq!(
            identity.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(identity.id, 0xba7816bf8f0);
    }
}
