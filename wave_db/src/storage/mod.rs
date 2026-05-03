//! On-disk storage primitives for the WaveDB disk-backed store.
//!
//! | Module           | Responsibility                                      |
//! |------------------|-----------------------------------------------------|
//! | [`codec`]        | Binary encode/decode for records and anchor slots   |
//! | [`page_format`]  | Slotted-page layout (in-memory page representation) |
//! | [`data_file`]    | Hash-mapped paged file (anchors + versioned records)|
//! | [`journal_file`] | Append-only journal for crash recovery              |
//! | [`heap_file`]    | 4 KiB-aligned overflow heap for large payloads      |
//! | [`index_file`]   | Sorted creation-order index per (STRUCT_ID, TENANT) |

pub mod codec;
pub mod data_file;
pub mod heap_file;
pub mod index_file;
pub mod journal_file;
pub mod page_format;
