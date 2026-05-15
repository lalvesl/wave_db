use wavedb_core::{Id, Metadata};
use wavedb_macros::wave_db;

/// This test should fail because struct_id is missing.
#[wave_db(NonUnique)]
pub struct Foo1 {
}

fn main() {}
