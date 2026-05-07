//! Binary encode/decode helpers for all on-disk record formats.
//!
//! # Wire formats
//!
//! **Versioned record** (tag `0x02`):
//! ```text
//! [u8 tag=0x02][u128 id][u128 old_mod_id][u128 new_mod_id]
//! [u64 creator_and_version][u32 data_len][data bytes]
//! ```
//! Fixed overhead: 1 + 16 + 16 + 16 + 8 + 4 = 61 bytes.
//!
//! **Anchor slot** (tag `0x01`):
//! ```text
//! [u8 tag=0x01][u8 type: 0=Inline 1=PointerOnly 2=Tombstone]
//! [u128 version_id]
//! [u32 data_len][data bytes]   ← 0 for PointerOnly / Tombstone
//! [u32 ref_count][ref_count × u128 ref_ids]
//! ```
//! Fixed overhead: 1 + 1 + 16 + 4 + 4 = 26 bytes.

use crate::{
    anchor::{AnchorContent, AnchorSlot},
    id::{Id, Timestamp},
    metadata::{CreatorAndVersion, Metadata},
    store::StoredRecord,
};

pub const TAG_ANCHOR: u8 = 0x01;
pub const TAG_VERSIONED: u8 = 0x02;

const ANCHOR_TYPE_INLINE: u8 = 0;
const ANCHOR_TYPE_POINTER: u8 = 1;
const ANCHOR_TYPE_TOMBSTONE: u8 = 2;

// ─── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    UnexpectedEof,
    UnknownTag(u8),
    UnknownAnchorType(u8),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedEof => write!(f, "unexpected end of encoded bytes"),
            Self::UnknownTag(t) => write!(f, "unknown record tag: {t:#04x}"),
            Self::UnknownAnchorType(t) => write!(f, "unknown anchor type byte: {t:#04x}"),
        }
    }
}

impl std::error::Error for CodecError {}

// ─── Low-level encode ─────────────────────────────────────────────────────────

#[inline]
fn push_u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
}

#[inline]
fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn push_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn push_u128(buf: &mut Vec<u8>, v: u128) {
    buf.extend_from_slice(&v.to_le_bytes());
}

// ─── Low-level decode ─────────────────────────────────────────────────────────

#[inline]
fn read_u8(src: &[u8], pos: &mut usize) -> Result<u8, CodecError> {
    if *pos >= src.len() {
        return Err(CodecError::UnexpectedEof);
    }
    let v = src[*pos];
    *pos += 1;
    Ok(v)
}

#[inline]
fn read_u32(src: &[u8], pos: &mut usize) -> Result<u32, CodecError> {
    let end = *pos + 4;
    if end > src.len() {
        return Err(CodecError::UnexpectedEof);
    }
    let v = u32::from_le_bytes(src[*pos..end].try_into().unwrap());
    *pos = end;
    Ok(v)
}

#[inline]
fn read_u64(src: &[u8], pos: &mut usize) -> Result<u64, CodecError> {
    let end = *pos + 8;
    if end > src.len() {
        return Err(CodecError::UnexpectedEof);
    }
    let v = u64::from_le_bytes(src[*pos..end].try_into().unwrap());
    *pos = end;
    Ok(v)
}

#[inline]
fn read_u128(src: &[u8], pos: &mut usize) -> Result<u128, CodecError> {
    let end = *pos + 16;
    if end > src.len() {
        return Err(CodecError::UnexpectedEof);
    }
    let v = u128::from_le_bytes(src[*pos..end].try_into().unwrap());
    *pos = end;
    Ok(v)
}

#[inline]
fn read_bytes(src: &[u8], pos: &mut usize, len: usize) -> Result<Vec<u8>, CodecError> {
    let end = *pos + len;
    if end > src.len() {
        return Err(CodecError::UnexpectedEof);
    }
    let v = src[*pos..end].to_vec();
    *pos = end;
    Ok(v)
}

// ─── Anchor sentinel ID ───────────────────────────────────────────────────────

/// Construct the stable data-file key for the anchor slot of a record.
///
/// Anchors are stored with `created_at = 0` and `slider = 0`. Because the
/// WaveDB epoch is 2025-01-01, any real record has `created_at > 0`, so this
/// is an unambiguous discriminator in `scan_all`.
pub fn anchor_sentinel_id(rec_id: Id) -> Id {
    Id::new(
        rec_id.tenant_id(),
        rec_id.shard_id(),
        rec_id.struct_id(),
        Timestamp::from_ticks(0),
        crate::id::Slider::new(0),
    )
}

/// Returns `true` if `id` is an anchor sentinel (created_at == 0, slider == 0).
pub fn is_anchor_sentinel(id: Id) -> bool {
    id.created_at().ticks() == 0 && id.slider().value() == 0
}

// ─── Versioned record ─────────────────────────────────────────────────────────

/// Serialize a [`StoredRecord`] to bytes (tag `0x02`).
pub fn encode_versioned_record(rec: &StoredRecord) -> Vec<u8> {
    let mut buf = Vec::with_capacity(61 + rec.data.len());
    push_u8(&mut buf, TAG_VERSIONED);
    push_u128(&mut buf, rec.id.raw());
    push_u128(&mut buf, rec.metadata.old_modification_id.raw());
    push_u128(&mut buf, rec.metadata.new_modification_id.raw());
    push_u64(&mut buf, rec.metadata.creator_and_version.raw());
    push_u32(&mut buf, rec.data.len() as u32);
    buf.extend_from_slice(&rec.data);
    buf
}

/// Deserialize a [`StoredRecord`] from bytes produced by [`encode_versioned_record`].
pub fn decode_versioned_record(src: &[u8]) -> Result<StoredRecord, CodecError> {
    let mut pos = 0;
    let tag = read_u8(src, &mut pos)?;
    if tag != TAG_VERSIONED {
        return Err(CodecError::UnknownTag(tag));
    }
    let id = Id::from_raw(read_u128(src, &mut pos)?);
    let old_mod_id = Id::from_raw(read_u128(src, &mut pos)?);
    let new_mod_id = Id::from_raw(read_u128(src, &mut pos)?);
    let cav = CreatorAndVersion::from_raw(read_u64(src, &mut pos)?);
    let data_len = read_u32(src, &mut pos)? as usize;
    let data = read_bytes(src, &mut pos, data_len)?;

    Ok(StoredRecord {
        id,
        metadata: Metadata {
            old_modification_id: old_mod_id,
            new_modification_id: new_mod_id,
            creator_and_version: cav,
        },
        data,
    })
}

// ─── Anchor slot ──────────────────────────────────────────────────────────────

/// Serialize an [`AnchorSlot`] to bytes (tag `0x01`).
pub fn encode_anchor_slot(slot: &AnchorSlot) -> Vec<u8> {
    let (anchor_type, version_id, data): (u8, Id, &[u8]) = match &slot.content {
        AnchorContent::Inline { data, current_version_id } => {
            (ANCHOR_TYPE_INLINE, *current_version_id, data.as_slice())
        }
        AnchorContent::PointerOnly { current_version_id } => {
            (ANCHOR_TYPE_POINTER, *current_version_id, &[])
        }
        AnchorContent::Tombstone { final_version_id } => {
            (ANCHOR_TYPE_TOMBSTONE, *final_version_id, &[])
        }
    };

    let mut buf =
        Vec::with_capacity(26 + data.len() + slot.inbound_refs.len() * 16);
    push_u8(&mut buf, TAG_ANCHOR);
    push_u8(&mut buf, anchor_type);
    push_u128(&mut buf, version_id.raw());
    push_u32(&mut buf, data.len() as u32);
    buf.extend_from_slice(data);
    push_u32(&mut buf, slot.inbound_refs.len() as u32);
    for r in &slot.inbound_refs {
        push_u128(&mut buf, r.raw());
    }
    buf
}

/// Deserialize an [`AnchorSlot`] from bytes produced by [`encode_anchor_slot`].
pub fn decode_anchor_slot(src: &[u8]) -> Result<AnchorSlot, CodecError> {
    let mut pos = 0;
    let tag = read_u8(src, &mut pos)?;
    if tag != TAG_ANCHOR {
        return Err(CodecError::UnknownTag(tag));
    }
    let anchor_type = read_u8(src, &mut pos)?;
    let version_id = Id::from_raw(read_u128(src, &mut pos)?);
    let data_len = read_u32(src, &mut pos)? as usize;
    let data = read_bytes(src, &mut pos, data_len)?;
    let ref_count = read_u32(src, &mut pos)? as usize;
    let mut inbound_refs = Vec::with_capacity(ref_count);
    for _ in 0..ref_count {
        inbound_refs.push(Id::from_raw(read_u128(src, &mut pos)?));
    }

    let content = match anchor_type {
        ANCHOR_TYPE_INLINE => AnchorContent::Inline { data, current_version_id: version_id },
        ANCHOR_TYPE_POINTER => AnchorContent::PointerOnly { current_version_id: version_id },
        ANCHOR_TYPE_TOMBSTONE => AnchorContent::Tombstone { final_version_id: version_id },
        other => return Err(CodecError::UnknownAnchorType(other)),
    };

    Ok(AnchorSlot { content, inbound_refs })
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        anchor::AnchorContent,
        id::{ShardId, Slider, StructId, TenantId, Timestamp},
        metadata::Metadata,
    };

    fn make_id(ts: u64) -> Id {
        Id::new(
            TenantId::new(1),
            ShardId::new(0),
            StructId::new(2),
            Timestamp::from_ticks(ts),
            Slider::new(0),
        )
    }

    #[test]
    fn versioned_roundtrip() {
        let id = make_id(100);
        let rec = StoredRecord {
            id,
            metadata: Metadata::for_mutation(make_id(50), 3, 99),
            data: b"hello world".to_vec(),
        };
        let bytes = encode_versioned_record(&rec);
        let decoded = decode_versioned_record(&bytes).unwrap();
        assert_eq!(decoded.id, rec.id);
        assert_eq!(decoded.metadata.old_modification_id, rec.metadata.old_modification_id);
        assert_eq!(decoded.metadata.new_modification_id, rec.metadata.new_modification_id);
        assert_eq!(decoded.metadata.creator_id(), rec.metadata.creator_id());
        assert_eq!(decoded.data, rec.data);
    }

    #[test]
    fn anchor_inline_roundtrip() {
        let id = make_id(200);
        let slot = AnchorSlot {
            content: AnchorContent::Inline {
                data: b"inline bytes".to_vec(),
                current_version_id: id,
            },
            inbound_refs: vec![make_id(300), make_id(400)],
        };
        let bytes = encode_anchor_slot(&slot);
        let decoded = decode_anchor_slot(&bytes).unwrap();
        assert!(matches!(decoded.content, AnchorContent::Inline { .. }));
        assert_eq!(decoded.inbound_refs.len(), 2);
    }

    #[test]
    fn anchor_tombstone_roundtrip() {
        let id = make_id(500);
        let mut slot = AnchorSlot::new_inline(b"data".to_vec(), id);
        slot.tombstone(id);
        let bytes = encode_anchor_slot(&slot);
        let decoded = decode_anchor_slot(&bytes).unwrap();
        assert!(matches!(decoded.content, AnchorContent::Tombstone { .. }));
    }

    #[test]
    fn anchor_pointer_roundtrip() {
        let id = make_id(600);
        let slot = AnchorSlot::new_pointer(id);
        let bytes = encode_anchor_slot(&slot);
        let decoded = decode_anchor_slot(&bytes).unwrap();
        assert!(matches!(decoded.content, AnchorContent::PointerOnly { .. }));
    }

    #[test]
    fn is_anchor_sentinel_works() {
        let tenant = TenantId::new(0xCAFE);
        let shard = ShardId::new(1);
        let sid = StructId::new(7);
        let sentinel = anchor_sentinel_id(Id::new(tenant, shard, sid, Timestamp::from_ticks(100), Slider::new(5)));
        assert!(is_anchor_sentinel(sentinel));
        // A real record is not a sentinel
        let real = Id::new(tenant, shard, sid, Timestamp::from_ticks(1), Slider::new(0));
        assert!(!is_anchor_sentinel(real));
    }

    #[test]
    fn wrong_tag_returns_error() {
        let id = make_id(700);
        let rec = StoredRecord::new_first(id, 1, 0, b"x".to_vec());
        let bytes = encode_versioned_record(&rec);
        // Try to decode as anchor
        assert!(decode_anchor_slot(&bytes).is_err());
    }
}
