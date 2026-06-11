use wavedb_macros::wave_db;

#[wave_db(struct_id = 23, NonUnique)]
pub struct Document1 {
    pub title: String,
    pub body: Vec<u8>,
    pub thumbnail: Option<Vec<u8>>,
    pub subtitle: Option<String>,
    pub author: u64,
    pub stars: u32,
}

fn main() {
    use wavedb_core::WaveDbStruct;
    // String / Vec / Option<Vec> / Option<String> are heapable; fixed-width
    // fields (author, stars) and the injected id/metadata are stackable.
    assert_eq!(
        Document1::HEAPABLE_FIELDS,
        &["title", "body", "thumbnail", "subtitle"],
    );
}
