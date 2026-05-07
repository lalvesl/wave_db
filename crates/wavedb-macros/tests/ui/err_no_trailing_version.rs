use wavedb_core::{Id, Metadata};
use wavedb_macros::wave_db;

/// This test should fail because the struct name has no trailing version number.
#[wave_db(struct_id = 1)]
pub struct Foo {
    pub id: Id,
    pub metadata: Metadata,
}

fn main() {}
