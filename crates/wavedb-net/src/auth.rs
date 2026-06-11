//! Cluster-key based HMAC-SHA256 authentication for node-to-node communication.
//!
//! Both Quick-Node gossip and Quick-Node→Slow-Node flush use the same token
//! format. A [`ClusterKey`] is a 32-byte shared secret deployed to every node
//! in the cluster; tokens are short-lived (±30 s replay window) and
//! purpose-scoped so a gossip token cannot be replayed against the flush endpoint.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use wavedb_macros::WaveWire;

type HmacSha256 = Hmac<Sha256>;

/// Maximum clock skew tolerated when verifying a token (seconds each direction).
const WINDOW_SECS: u64 = 30;

/// Identifies which protocol a [`NodeToken`] was minted for.
///
/// The explicit discriminants feed the HMAC input (`purpose as u8`); the wire
/// tag is the ordinal index, independent of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, WaveWire)]
#[repr(u8)]
pub enum TokenPurpose {
    /// Gossip announce/withdraw between Quick-Nodes.
    Gossip = 0x01,
    /// History flush from Quick-Node to Slow-Node.
    Flush = 0x02,
    /// Metrics poll from the monitor to any node.
    Monitor = 0x03,
}

/// A short-lived HMAC-SHA256 proof of cluster membership.
///
/// Carried alongside gossip and flush messages; verified by the receiver using
/// the shared [`ClusterKey`].
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub struct NodeToken {
    /// FNV-1a hash of the sending node's listen address.
    pub node_id: u64,
    /// Unix timestamp (seconds) when this token was minted.
    pub timestamp_secs: u64,
    /// HMAC-SHA256 over `node_id ‖ timestamp_secs ‖ purpose`.
    pub tag: [u8; 32],
}

/// 32-byte shared cluster secret used to mint and verify [`NodeToken`]s.
///
/// Constructed from a 64-hex-character string via [`ClusterKey::from_hex`].
/// The key bytes are never printed — `Debug` emits a redacted placeholder.
#[derive(Clone)]
pub struct ClusterKey([u8; 32]);

impl std::fmt::Debug for ClusterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClusterKey(<redacted>)")
    }
}

impl ClusterKey {
    /// Decode a 64-hex-character string into a `ClusterKey`.
    pub fn from_hex(hex: &str) -> Result<Self, &'static str> {
        if hex.len() != 64 {
            return Err(
                "cluster key must be exactly 64 hex characters (32 bytes)",
            );
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            bytes[i] = (hex_nibble(chunk[0])? << 4) | hex_nibble(chunk[1])?;
        }
        Ok(Self(bytes))
    }

    /// Mint a token valid at the given Unix `timestamp_secs`.
    pub fn mint_at(
        &self,
        node_id: u64,
        purpose: TokenPurpose,
        timestamp_secs: u64,
    ) -> NodeToken {
        let tag = compute_tag(&self.0, node_id, timestamp_secs, purpose);
        NodeToken {
            node_id,
            timestamp_secs,
            tag,
        }
    }

    /// Mint a token using the current wall-clock time.
    pub fn mint(&self, node_id: u64, purpose: TokenPurpose) -> NodeToken {
        self.mint_at(node_id, purpose, now_secs())
    }

    /// Verify a token against the current wall-clock time.
    ///
    /// Returns `false` if the timestamp falls outside the ±30 s window or the
    /// HMAC tag does not match (constant-time comparison).
    pub fn verify(&self, token: &NodeToken, purpose: TokenPurpose) -> bool {
        self.verify_at(token, purpose, now_secs())
    }

    /// Verify a token against an explicit `now_secs` (useful in tests).
    pub fn verify_at(
        &self,
        token: &NodeToken,
        purpose: TokenPurpose,
        now_secs: u64,
    ) -> bool {
        let ts = token.timestamp_secs;
        let skew = now_secs.max(ts) - now_secs.min(ts);
        if skew > WINDOW_SECS {
            return false;
        }
        let expected = compute_tag(&self.0, token.node_id, ts, purpose);
        // Constant-time byte comparison — prevents timing side-channels.
        let mut diff = 0u8;
        for (&a, &b) in expected.iter().zip(token.tag.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

fn compute_tag(
    key: &[u8; 32],
    node_id: u64,
    timestamp_secs: u64,
    purpose: TokenPurpose,
) -> [u8; 32] {
    let mut mac =
        HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&node_id.to_le_bytes());
    mac.update(&timestamp_secs.to_le_bytes());
    mac.update(&[purpose as u8]);
    let result = mac.finalize().into_bytes();
    let mut tag = [0u8; 32];
    tag.copy_from_slice(&result);
    tag
}

const fn hex_nibble(b: u8) -> Result<u8, &'static str> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err("invalid hex character in cluster key"),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const HEX_KEY: &str =
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn key() -> ClusterKey {
        ClusterKey::from_hex(HEX_KEY).unwrap()
    }

    #[test]
    fn from_hex_valid() {
        assert!(ClusterKey::from_hex(HEX_KEY).is_ok());
    }

    #[test]
    fn from_hex_too_short_errors() {
        assert!(ClusterKey::from_hex("deadbeef").is_err());
    }

    #[test]
    fn from_hex_invalid_char_errors() {
        // First byte is 'zz' — not valid hex but correct length.
        let bad =
            "zz0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        assert!(ClusterKey::from_hex(bad).is_err());
    }

    #[test]
    fn mint_and_verify_roundtrip() {
        let k = key();
        let token = k.mint_at(42, TokenPurpose::Gossip, 1_000_000);
        assert!(k.verify_at(&token, TokenPurpose::Gossip, 1_000_000));
    }

    #[test]
    fn verify_within_window() {
        let k = key();
        let token = k.mint_at(1, TokenPurpose::Flush, 1_000_000);
        assert!(k.verify_at(&token, TokenPurpose::Flush, 1_000_029));
        assert!(k.verify_at(&token, TokenPurpose::Flush, 1_000_000 - 29));
    }

    #[test]
    fn verify_outside_window_fails() {
        let k = key();
        let token = k.mint_at(1, TokenPurpose::Gossip, 1_000_000);
        assert!(!k.verify_at(&token, TokenPurpose::Gossip, 1_000_031));
        assert!(!k.verify_at(&token, TokenPurpose::Gossip, 999_969));
    }

    #[test]
    fn wrong_purpose_fails() {
        let k = key();
        let token = k.mint_at(1, TokenPurpose::Gossip, 1_000_000);
        assert!(!k.verify_at(&token, TokenPurpose::Flush, 1_000_000));
    }

    #[test]
    fn tampered_tag_fails() {
        let k = key();
        let mut token = k.mint_at(99, TokenPurpose::Gossip, 1_000_000);
        token.tag[0] ^= 0xFF;
        assert!(!k.verify_at(&token, TokenPurpose::Gossip, 1_000_000));
    }

    #[test]
    fn different_node_id_fails() {
        let k = key();
        let token = k.mint_at(1, TokenPurpose::Gossip, 1_000_000);
        let spoofed = NodeToken {
            node_id: 999,
            ..token
        };
        assert!(!k.verify_at(&spoofed, TokenPurpose::Gossip, 1_000_000));
    }

    #[test]
    fn debug_redacts_key_bytes() {
        let k = key();
        let s = format!("{k:?}");
        assert!(!s.contains("000102"));
        assert!(s.contains("redacted"));
    }

    #[test]
    fn token_wire_roundtrip() {
        let k = key();
        let token = k.mint_at(77, TokenPurpose::Flush, 9_999_999);
        let bytes = wavedb_core::wire::to_wire(&token).unwrap();
        let decoded: NodeToken = wavedb_core::wire::from_wire(&bytes).unwrap();
        assert_eq!(decoded, token);
    }
}
