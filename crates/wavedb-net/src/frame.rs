//! Postcard-based framing for `WaveDB` network messages.
//!
//! Every message over the wire (HTTP body or WebSocket text/binary frame) is a
//! length-prefixed postcard envelope:
//!
//! ```text
//! [ u32 le length ] [ postcard bytes … ]
//! ```
//!
//! The 4-byte little-endian length field lets streaming parsers on both sides
//! know exactly how many bytes to read before attempting to deserialise.
//!
//! Both [`encode`] and [`decode`] work on `&[u8]` slices; callers provide the
//! buffer management.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use wavedb_core::Result;

// ── Framing constants ────────────────────────────────────────────────────────

/// Number of bytes occupied by the length prefix.
pub const LENGTH_PREFIX_BYTES: usize = 4;

// ── Encode ───────────────────────────────────────────────────────────────────

/// Encode `value` into a framed byte buffer.
///
/// The resulting buffer begins with a 4-byte LE length followed by the
/// postcard-serialised payload.
pub fn encode<T: Serialize>(value: &T) -> Result<Bytes> {
    let payload = postcard::to_allocvec(value)?;
    let mut buf = BytesMut::with_capacity(LENGTH_PREFIX_BYTES + payload.len());
    #[allow(clippy::cast_possible_truncation)]
    buf.put_u32_le(payload.len() as u32);
    buf.put_slice(&payload);
    Ok(buf.freeze())
}

// ── Decode ───────────────────────────────────────────────────────────────────

/// Decode one framed message from `buf`.
///
/// Returns `Ok(Some(value))` when a complete frame is available,
/// `Ok(None)` when more bytes are needed, and `Err` on corruption.
///
/// On success the consumed bytes are advanced in `buf`.
pub fn decode<T: for<'de> Deserialize<'de>>(buf: &mut BytesMut) -> Result<Option<T>> {
    if buf.len() < LENGTH_PREFIX_BYTES {
        return Ok(None);
    }

    // Peek the length without advancing.
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

    if buf.len() < LENGTH_PREFIX_BYTES + len {
        return Ok(None); // not enough data yet
    }

    buf.advance(LENGTH_PREFIX_BYTES);
    let payload = buf.split_to(len);
    let value: T = postcard::from_bytes(&payload)?;
    Ok(Some(value))
}

/// Decode a complete (already-framed) byte slice, skipping the length prefix.
///
/// Useful when the transport layer has already separated the frame for us
/// (e.g. WebSocket message boundaries).
pub fn decode_payload<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Result<T> {
    Ok(postcard::from_bytes(payload)?)
}

/// Encode `value` without the length prefix (for WebSocket frames, where the
/// transport layer delimits messages).
pub fn encode_payload<T: Serialize>(value: &T) -> Result<Bytes> {
    Ok(Bytes::from(postcard::to_allocvec(value)?))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Msg {
        x: u32,
        s: String,
    }

    #[test]
    fn roundtrip_framed() {
        let msg = Msg { x: 42, s: "hello".into() };
        let framed = encode(&msg).unwrap();
        let mut buf = BytesMut::from(framed.as_ref());
        let decoded: Msg = decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg, decoded);
        assert!(buf.is_empty(), "buf should be fully consumed");
    }

    #[test]
    fn partial_frame_returns_none() {
        let msg = Msg { x: 7, s: "partial".into() };
        let framed = encode(&msg).unwrap();
        // Give only half the bytes.
        let mut buf = BytesMut::from(&framed[..framed.len() / 2]);
        let result: Option<Msg> = decode(&mut buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn two_frames_in_one_buffer() {
        let m1 = Msg { x: 1, s: "one".into() };
        let m2 = Msg { x: 2, s: "two".into() };
        let mut combined = BytesMut::new();
        combined.extend_from_slice(&encode(&m1).unwrap());
        combined.extend_from_slice(&encode(&m2).unwrap());

        let d1: Msg = decode(&mut combined).unwrap().unwrap();
        let d2: Msg = decode(&mut combined).unwrap().unwrap();
        assert_eq!(m1, d1);
        assert_eq!(m2, d2);
        assert!(combined.is_empty());
    }

    #[test]
    fn unframed_payload_roundtrip() {
        let msg = Msg { x: 99, s: "payload".into() };
        let bytes = encode_payload(&msg).unwrap();
        let decoded: Msg = decode_payload(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }
}
