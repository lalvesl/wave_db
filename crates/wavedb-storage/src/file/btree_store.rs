//! Persistent B+ tree nodes in `data.bin`, with an in-memory node cache.
//!
//! A B+ tree lives in the "infinity" address space: nodes are small (a block or
//! two) but there can be billions of them, addressed by a [`NodeRef`] — `u48`
//! start block + `u16` block count packed in a `u64` (the same shape as
//! [`DictRef`](crate::file::dict::DictRef)). Each node is a CRC-checked page;
//! edits are **copy-on-write** (write the new node, repoint the parent, free the
//! old), exactly like every other page.
//!
//! [`NodeCache`] is the in-memory cache the tree reads through: `NodeRef → Node`.
//! A miss reads the node page from `data.bin` and caches it; a write caches the
//! fresh node. This module is the **node persistence layer only** — the B+ tree
//! search / insert / split logic builds on top of it (next unit).
//!
//! # Node page format
//!
//! ```text
//! [ crc32 u32 ][ node_type u8 ][ entry_count u16 ]              ← 7-byte header
//! Leaf:     [ next: NodeRef u64 ][ (key u64)(value u128) × count ]
//! Internal: [ key u64 × count ][ child: NodeRef u64 × (count + 1) ]
//! ```
//!
//! padded to the block boundary. `crc32` covers the header (from `node_type`)
//! through the body; the body length is derived from `node_type` + `count`, so
//! trailing padding is ignored on read.

use crate::file::block_alloc::{BlockRun, blocks_for_bytes};
use crate::file::block_file::BlockFile;
use crate::{StorageError, StorageResult};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Bits of a [`NodeRef`] holding the start block (`u48`); the high `u16` is the
/// block count.
const START_BITS: u32 = 48;
const START_MASK: u64 = (1u64 << START_BITS) - 1;

/// node page header: `crc32(4) + node_type(1) + entry_count(2)`.
const NODE_HEADER: usize = 7;

const TYPE_LEAF: u8 = 0;
const TYPE_INTERNAL: u8 = 1;

/// A self-locating pointer to a B+ tree node: `u48` start block + `u16` block
/// count. `raw() == 0` ⇒ no node (e.g. a rightmost leaf's `next`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NodeRef(u64);

impl NodeRef {
    /// The "no node" ref.
    pub const NONE: Self = Self(0);

    /// Pack a `(start_block, block_count)` into a ref.
    ///
    /// # Panics
    ///
    /// Debug-asserts `start_block` fits `u48`.
    #[must_use]
    pub fn new(start_block: u64, block_count: u16) -> Self {
        debug_assert!(start_block <= START_MASK, "node start exceeds u48");
        Self(start_block | (u64::from(block_count) << START_BITS))
    }

    /// The packed `u64` (stored inside a parent node).
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Unpack from a stored `u64`.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Whether this is the "no node" ref.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Start block of the node's run.
    #[must_use]
    pub const fn start_block(self) -> u64 {
        self.0 & START_MASK
    }

    /// Block count of the node's run.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn block_count(self) -> u16 {
        (self.0 >> START_BITS) as u16
    }

    /// The block run holding the node.
    #[must_use]
    pub const fn run(self) -> BlockRun {
        BlockRun {
            start: self.start_block(),
            len: self.block_count() as u64,
        }
    }
}

/// A B+ tree node: a leaf of `(key, value)` pairs, or an internal node of
/// separator keys and child pointers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// Sorted `(key → value-anchor)` pairs, plus the next leaf for range scans.
    Leaf {
        /// `(key u64, value/anchor u128)`, key-sorted.
        entries: Vec<(u64, u128)>,
        /// The next leaf to the right ([`NodeRef::NONE`] at the end).
        next: NodeRef,
    },
    /// `keys.len()` separators and `keys.len() + 1` children.
    Internal {
        /// Separator keys, sorted.
        keys: Vec<u64>,
        /// Child pointers; `children.len() == keys.len() + 1`.
        children: Vec<NodeRef>,
    },
}

impl Node {
    /// Body length (after the 7-byte header) for this node's `count`.
    fn body_len(&self) -> usize {
        match self {
            Self::Leaf { entries, .. } => 8 + entries.len() * 24,
            Self::Internal { keys, children } => keys.len() * 8 + children.len() * 8,
        }
    }

    /// Encode the node to a CRC-checked page image.
    fn encode(&self) -> Vec<u8> {
        let total = NODE_HEADER + self.body_len();
        let mut buf = vec![0u8; total];
        match self {
            Self::Leaf { entries, next } => {
                buf[4] = TYPE_LEAF;
                buf[5..7].copy_from_slice(
                    &u16::try_from(entries.len())
                        .expect("leaf entries exceed u16")
                        .to_le_bytes(),
                );
                buf[7..15].copy_from_slice(&next.raw().to_le_bytes());
                let mut off = 15;
                for (key, value) in entries {
                    buf[off..off + 8].copy_from_slice(&key.to_le_bytes());
                    off += 8;
                    buf[off..off + 16].copy_from_slice(&value.to_le_bytes());
                    off += 16;
                }
            }
            Self::Internal { keys, children } => {
                buf[4] = TYPE_INTERNAL;
                buf[5..7].copy_from_slice(
                    &u16::try_from(keys.len())
                        .expect("internal keys exceed u16")
                        .to_le_bytes(),
                );
                let mut off = NODE_HEADER;
                for key in keys {
                    buf[off..off + 8].copy_from_slice(&key.to_le_bytes());
                    off += 8;
                }
                for child in children {
                    buf[off..off + 8].copy_from_slice(&child.raw().to_le_bytes());
                    off += 8;
                }
            }
        }
        let crc = crc32fast::hash(&buf[4..total]);
        buf[0..4].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Decode a node page image, verifying its CRC.
    fn decode(bytes: &[u8]) -> StorageResult<Self> {
        if bytes.len() < NODE_HEADER {
            return Err(StorageError::Other("node page too short".into()));
        }
        let node_type = bytes[4];
        let count = u16::from_le_bytes(bytes[5..7].try_into().unwrap()) as usize;
        let body_len = match node_type {
            TYPE_LEAF => 8 + count * 24,
            TYPE_INTERNAL => count * 8 + (count + 1) * 8,
            other => {
                return Err(StorageError::Other(format!(
                    "unknown node type {other}"
                )));
            }
        };
        let end = NODE_HEADER + body_len;
        if bytes.len() < end {
            return Err(StorageError::Other("node page length out of range".into()));
        }
        let expected = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let actual = crc32fast::hash(&bytes[4..end]);
        if expected != actual {
            return Err(StorageError::ChecksumMismatch {
                page_index: 0,
                expected,
                actual,
            });
        }

        if node_type == TYPE_LEAF {
            let next = NodeRef::from_raw(u64::from_le_bytes(
                bytes[7..15].try_into().unwrap(),
            ));
            let mut entries = Vec::with_capacity(count);
            let mut off = 15;
            for _ in 0..count {
                let key = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
                off += 8;
                let value =
                    u128::from_le_bytes(bytes[off..off + 16].try_into().unwrap());
                off += 16;
                entries.push((key, value));
            }
            Ok(Self::Leaf { entries, next })
        } else {
            let mut off = NODE_HEADER;
            let mut keys = Vec::with_capacity(count);
            for _ in 0..count {
                keys.push(u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap()));
                off += 8;
            }
            let mut children = Vec::with_capacity(count + 1);
            for _ in 0..=count {
                children.push(NodeRef::from_raw(u64::from_le_bytes(
                    bytes[off..off + 8].try_into().unwrap(),
                )));
                off += 8;
            }
            Ok(Self::Internal { keys, children })
        }
    }
}

/// In-memory B+ tree node cache over a shared [`BlockFile`]: `NodeRef → Node`.
///
/// The tree reads through this — a miss reads the node page from `data.bin`; a
/// write caches the fresh node. Copy-on-write edits [`invalidate`](Self::invalidate)
/// the old ref after repointing the parent.
pub struct NodeCache {
    cache: RwLock<HashMap<u64, Arc<Node>>>,
}

impl Default for NodeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Number of cached nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cache.read().len()
    }

    /// Whether the cache holds no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cache.read().is_empty()
    }

    /// Load the node at `node_ref` — cache hit, else read its page from
    /// `data.bin` and cache it.
    ///
    /// # Panics
    ///
    /// Debug-asserts `node_ref` is not [`NodeRef::NONE`].
    pub fn load(
        &self,
        file: &BlockFile,
        node_ref: NodeRef,
    ) -> StorageResult<Arc<Node>> {
        debug_assert!(!node_ref.is_none(), "load of the empty node ref");
        if let Some(node) = self.cache.read().get(&node_ref.raw()) {
            return Ok(Arc::clone(node));
        }
        let node = Arc::new(Node::decode(&file.read_run(node_ref.run())?)?);
        self.cache.write().insert(node_ref.raw(), Arc::clone(&node));
        Ok(node)
    }

    /// Write `node` to a fresh extent (copy-on-write) and cache it. The caller
    /// repoints the parent at the returned ref and journals the allocation, then
    /// [`invalidate`](Self::invalidate)s + frees the old ref.
    pub fn write(
        &self,
        file: &BlockFile,
        node: &Node,
    ) -> StorageResult<NodeRef> {
        let image = node.encode();
        let blocks = blocks_for_bytes(image.len() as u64);
        let count = u16::try_from(blocks)
            .map_err(|_| StorageError::Other("node exceeds u16 blocks".into()))?;
        let run = file.allocate(blocks)?;
        file.write_run(run, &image)?;
        let node_ref = NodeRef::new(run.start, count);
        self.cache
            .write()
            .insert(node_ref.raw(), Arc::new(node.clone()));
        Ok(node_ref)
    }

    /// Drop a node from the cache (its extent has been freed).
    pub fn invalidate(&self, node_ref: NodeRef) {
        self.cache.write().remove(&node_ref.raw());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn leaf() -> Node {
        Node::Leaf {
            entries: vec![(1, 100), (5, 500), (9, 900)],
            next: NodeRef::new(42, 1),
        }
    }

    fn internal() -> Node {
        Node::Internal {
            keys: vec![10, 20],
            children: vec![
                NodeRef::new(2, 1),
                NodeRef::new(3, 1),
                NodeRef::new(4, 1),
            ],
        }
    }

    #[test]
    fn node_ref_packs_and_unpacks() {
        let r = NodeRef::new(0x0001_2345_6789, 3);
        assert_eq!(r.start_block(), 0x0001_2345_6789);
        assert_eq!(r.block_count(), 3);
        assert_eq!(r.run(), BlockRun { start: 0x0001_2345_6789, len: 3 });
        assert!(!r.is_none());
        assert!(NodeRef::NONE.is_none());
        assert_eq!(NodeRef::from_raw(r.raw()), r);
    }

    #[test]
    fn leaf_and_internal_roundtrip() {
        for node in [leaf(), internal()] {
            let decoded = Node::decode(&node.encode()).unwrap();
            assert_eq!(decoded, node);
        }
    }

    #[test]
    fn corrupt_node_is_rejected() {
        let mut corrupt = leaf().encode();
        corrupt[NODE_HEADER + 2] ^= 0xFF; // flip a byte in the CRC region
        assert!(matches!(
            Node::decode(&corrupt),
            Err(StorageError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn cache_write_then_load_hits() {
        let bf = BlockFile::create_in_memory();
        let cache = NodeCache::new();
        let node = leaf();
        let r = cache.write(&bf, &node).unwrap();
        assert_eq!(cache.len(), 1);
        // Cache hit.
        assert_eq!(&*cache.load(&bf, r).unwrap(), &node);
    }

    #[test]
    fn cold_cache_loads_from_data_bin() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let node = internal();
        let r;
        {
            let bf = BlockFile::create(&path).unwrap();
            r = NodeCache::new().write(&bf, &node).unwrap();
            bf.sync().unwrap();
        }
        // Reopen with the node's run live; a fresh (cold) cache reads it back.
        let bf = BlockFile::open(&path, [r.run()]).unwrap();
        let cache = NodeCache::new();
        assert!(cache.is_empty());
        assert_eq!(&*cache.load(&bf, r).unwrap(), &node);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn invalidate_drops_cached_node() {
        let bf = BlockFile::create_in_memory();
        let cache = NodeCache::new();
        let r = cache.write(&bf, &leaf()).unwrap();
        cache.invalidate(r);
        assert!(cache.is_empty());
    }
}
