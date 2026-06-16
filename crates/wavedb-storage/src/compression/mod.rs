//! Compression: zstd for heap data + per-STRUCT dictionary compression.
//!
//! Dictionaries are stored in `data.bin` and located by
//! [`DictRef`](crate::file::dict::DictRef); the cache lives on the page
//! directory ([`DictStore`](crate::file::dict)).

pub mod heap_zstd;
pub mod page_codec;
