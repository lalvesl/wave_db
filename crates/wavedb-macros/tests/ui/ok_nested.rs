use wavedb_core::{Id, Metadata};
use wavedb_macros::wave_db;

#[wave_db(struct_id = 21, NestedNonUnique)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct InvoiceLine1 {
    pub id: Id,
    pub metadata: Metadata,
    pub product: u64,
    pub quantity: u32,
}

fn main() {
    use wavedb_core::WaveDbStruct;
    assert_eq!(InvoiceLine1::STRUCT_ID, 21);
    assert_eq!(InvoiceLine1::STRUCT_VERSION, 1);
    assert_eq!(InvoiceLine1::SHAPE, wavedb_core::Shape::NestedNonUnique);
}
