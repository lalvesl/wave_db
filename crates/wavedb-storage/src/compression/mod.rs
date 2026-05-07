//! Compression: zstd for heap data + per-STRUCT dictionary compression.

pub mod dict_cache;
pub mod heap_zstd;
pub mod page_codec;
