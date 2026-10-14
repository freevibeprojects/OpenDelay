// SPDX-License-Identifier: GPL-3.0-only
//! RTMP handshake (the plain version-3 form, RTMP 1.0 §5.2).
//!
//! The server answers with a zero "version" field in S1, which tells digest-
//! capable clients to fall back to the plain handshake. S2 echoes C1 and C2
//! echoes S1.

use std::hash::{BuildHasher, Hasher};

pub const VERSION: u8 = 3;
/// Size of C1/S1/C2/S2.
pub const SIZE: usize = 1536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeError(pub &'static str);

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RTMP handshake: {}", self.0)
    }
}
impl std::error::Error for HandshakeError {}

/// 1536 bytes: 4-byte time, 4 zero bytes, random filler (not security relevant).
fn random_block(time: u32) -> Vec<u8> {
    let mut seed = std::collections::hash_map::RandomState::new().build_hasher().finish() | 1;
    let mut block = Vec::with_capacity(SIZE);
    block.extend_from_slice(&time.to_be_bytes());
    block.extend_from_slice(&[0; 4]);
    while block.len() < SIZE {
        // xorshift64
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        block.extend_from_slice(&seed.to_le_bytes());
    }
    block.truncate(SIZE);
    block
}

/// Client: C0 + C1.
pub fn client_c0c1() -> Vec<u8> {
    let mut out = vec![VERSION];
    out.extend(random_block(0));
    out
}

/// Client: given S0 + S1 (1537 bytes), produce C2.
pub fn client_c2(s0s1: &[u8]) -> Result<Vec<u8>, HandshakeError> {
    if s0s1.len() != 1 + SIZE {
        return Err(HandshakeError("bad S0/S1 length"));
    }
    if s0s1[0] != VERSION {
        return Err(HandshakeError("server requested an unsupported RTMP version"));
    }
    Ok(s0s1[1..].to_vec())
}

/// Server: given C0 + C1 (1537 bytes), produce S0 + S1 + S2.
pub fn server_s0s1s2(c0c1: &[u8], uptime_ms: u32) -> Result<Vec<u8>, HandshakeError> {
    if c0c1.len() != 1 + SIZE {
        return Err(HandshakeError("bad C0/C1 length"));
    }
    // Version 6 (RTMPE) and others are not supported; 3 is the only plain version.
    if c0c1[0] != VERSION {
        return Err(HandshakeError("client requested an unsupported RTMP version"));
    }
    let mut out = Vec::with_capacity(1 + 2 * SIZE);
    out.push(VERSION);
    out.extend(random_block(uptime_ms));
    out.extend_from_slice(&c0c1[1..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_handshake_echoes() {
        let c0c1 = client_c0c1();
        assert_eq!(c0c1.len(), 1537);
        assert_eq!(c0c1[0], 3);
        assert_eq!(&c0c1[5..9], &[0, 0, 0, 0]);

        let s = server_s0s1s2(&c0c1, 42).unwrap();
        assert_eq!(s.len(), 1 + 2 * SIZE);
        assert_eq!(&s[1..5], &42u32.to_be_bytes());
        assert_eq!(&s[5..9], &[0, 0, 0, 0], "zero version selects the plain handshake");
        assert_eq!(&s[1 + SIZE..], &c0c1[1..], "S2 echoes C1");

        let c2 = client_c2(&s[..1 + SIZE]).unwrap();
        assert_eq!(&c2[..], &s[1..1 + SIZE], "C2 echoes S1");
    }

    #[test]
    fn rejects_bad_input() {
        let mut c = client_c0c1();
        c[0] = 6;
        assert!(server_s0s1s2(&c, 0).is_err());
        assert!(server_s0s1s2(&c[..100], 0).is_err());
        assert!(client_c2(&[3; 10]).is_err());
    }
}
