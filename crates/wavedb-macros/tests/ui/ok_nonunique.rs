use wavedb_core::{Id, Metadata};
use wavedb_macros::wave_db;

#[wave_db(struct_id = 7, NonUnique)]
pub struct Message42 {
    pub id: Id,
    pub metadata: Metadata,
    pub body: String,
}

fn main() {
    use wavedb_core::WaveDbStruct;
    assert_eq!(Message42::STRUCT_ID, 7);
    assert_eq!(Message42::STRUCT_VERSION, 42);
    assert_eq!(Message42::SHAPE, wavedb_core::Shape::NonUnique);
}
