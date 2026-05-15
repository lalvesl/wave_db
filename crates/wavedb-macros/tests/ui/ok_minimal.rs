use wavedb_core::{Id, Metadata};
use wavedb_macros::wave_db;

#[wave_db(struct_id = 1)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Foo1 {
    pub id: Id,
    pub metadata: Metadata,
    pub name: String,
}

fn main() {
    use wavedb_core::WaveDbStruct;
    assert_eq!(Foo1::STRUCT_ID, 1);
    assert_eq!(Foo1::STRUCT_VERSION, 1);
    assert_eq!(Foo1::SHAPE, wavedb_core::Shape::Unique);
}
