use wavedb_core::{Id, Metadata};
use wavedb_macros::wave_db;

#[wave_db(struct_id = 25, NonUnique, primary_anchor = username)]
pub struct User1 {
    pub id: Id,
    pub metadata: Metadata,
    pub username: String,
    pub email: String,
}

fn main() {
    use wavedb_core::WaveDbStruct;
    assert_eq!(User1::STRUCT_ID, 25);
    assert_eq!(User1::primary_anchor_field(), "username");
}
