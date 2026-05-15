use wavedb_core::{Id, Metadata};
use wavedb_macros::wave_db;

/// This test should fail because struct_id is too big for u20.
#[wave_db(struct_id = 1048576)]
pub struct Foo1 {
}

fn main() {}
