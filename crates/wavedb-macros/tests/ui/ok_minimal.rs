use wavedb_core::{Id, Metadata};
use wavedb_macros::wave_db;

#[wave_db(struct_id = 1)]
pub struct Foo1 {
    pub name: String,
}

fn main() {
    use wavedb_core::WaveDbStruct;
    assert_eq!(Foo1::STRUCT_ID, 1);
    assert_eq!(Foo1::STRUCT_VERSION, 1);
    assert_eq!(Foo1::SHAPE, wavedb_core::Shape::Unique);
}
