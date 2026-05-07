//! Journal & write pipeline: journaled cache that fronts the durable files.
//!
//! All writes go through the journal first, then a background drain.
//! Cache pressure causes write backpressure. Crash recovery replays
//! the journal on startup.

pub mod backpressure;
pub mod drain;
pub mod journal;
