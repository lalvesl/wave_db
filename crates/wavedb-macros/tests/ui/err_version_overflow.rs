use wavedb_core::{Id, Metadata};
use wavedb_macros::wave_db;

/// This test should fail because 300 > u8::MAX (255).
#[wave_db(struct_id = 1)]
pub struct Foo300 {
}

fn main() {}
