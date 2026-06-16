//! Page codec: encode/decode a page's stack data given its dictionary version.
//!
//! Each page header carries the dictionary version it was compressed with.
//! Old pages stay readable forever — the codec just needs the matching dict.

use crate::compression::heap_zstd;

/// zstd level for dictionary-coded page records. Pages are written once per
/// drain and read often, so trade encode time for ratio.
const PAGE_LEVEL: i32 = 19;

/// Encode page stack data (with optional dictionary-assisted compression).
///
/// For now, uses plain zstd. When dictionaries are trained, the dict bytes
/// will be prepended to the zstd context for better ratios.
pub fn encode_stack(
    data: &[u8],
    _dict_version: u32,
) -> crate::StorageResult<Vec<u8>> {
    heap_zstd::compress(data)
}

/// Decode page stack data.
pub fn decode_stack(
    compressed: &[u8],
    _dict_version: u32,
) -> crate::StorageResult<Vec<u8>> {
    heap_zstd::decompress(compressed)
}

/// Train a zstd dictionary from record-payload samples.
///
/// Homogeneous one-type pages share structure (the same fields, enums, key
/// names); a dictionary trained on a sample of their payloads captures it, so
/// each page — even a small one — compresses against the shared model instead
/// of carrying that structure itself. Returns the raw dictionary bytes (the
/// store keeps them as a page in `data.bin`, versioned per `(struct_id,
/// version)`).
///
/// `max_bytes` caps the dictionary size. zstd needs a reasonable volume of
/// samples relative to `max_bytes`; too few yields a weak (or empty) dict.
pub fn train_dictionary(
    samples: &[Vec<u8>],
    max_bytes: usize,
) -> crate::StorageResult<Vec<u8>> {
    Ok(zstd::dict::from_samples(samples, max_bytes)?)
}

/// Compress `data` against `dict`. An empty `dict` falls back to plain zstd, so
/// the caller can use one path whether or not a dictionary exists yet.
pub fn encode_with_dict(
    data: &[u8],
    dict: &[u8],
) -> crate::StorageResult<Vec<u8>> {
    if dict.is_empty() {
        return heap_zstd::compress_level(data, PAGE_LEVEL);
    }
    let mut compressor =
        zstd::bulk::Compressor::with_dictionary(PAGE_LEVEL, dict)?;
    Ok(compressor.compress(data)?)
}

/// Decompress `compressed` against `dict` (empty `dict` → plain zstd).
///
/// `capacity` is the known uncompressed length — the page header stores it, so
/// decode never has to guess a buffer size.
pub fn decode_with_dict(
    compressed: &[u8],
    dict: &[u8],
    capacity: usize,
) -> crate::StorageResult<Vec<u8>> {
    if dict.is_empty() {
        return heap_zstd::decompress(compressed);
    }
    let mut decompressor =
        zstd::bulk::Decompressor::with_dictionary(dict)?;
    Ok(decompressor.decompress(compressed, capacity)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_without_dict() {
        let data = b"struct field data for STRUCT_ID=7";
        let encoded = encode_stack(data, 0).unwrap();
        let decoded = decode_stack(&encoded, 0).unwrap();
        assert_eq!(&decoded, data);
    }

    #[test]
    fn different_dict_versions_still_decode() {
        // Both v1 and v2 should produce decodable output since we're
        // not yet using actual trained dictionaries.
        let data = b"test data";
        let v1 = encode_stack(data, 1).unwrap();
        let v2 = encode_stack(data, 2).unwrap();
        assert_eq!(decode_stack(&v1, 1).unwrap(), data);
        assert_eq!(decode_stack(&v2, 2).unwrap(), data);
    }

    // ── Dictionary codec ────────────────────────────────────────────────

    /// Homogeneous one-type records: same shape, varying values.
    fn homogeneous_samples(n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| {
                format!(
                    "{{\"struct_id\":7,\"tenant\":{},\"status\":\"active\",\
                     \"name\":\"user_{}\",\"role\":\"member\"}}",
                    i % 97,
                    i
                )
                .into_bytes()
            })
            .collect()
    }

    #[test]
    fn empty_dict_roundtrips() {
        let data = b"records blob with no dictionary trained yet";
        let enc = encode_with_dict(data, &[]).unwrap();
        assert_eq!(decode_with_dict(&enc, &[], data.len()).unwrap(), data);
    }

    #[test]
    fn trained_dict_roundtrips() {
        let samples = homogeneous_samples(2000);
        let dict = train_dictionary(&samples, 8 * 1024).unwrap();
        assert!(!dict.is_empty(), "training produced an empty dictionary");

        let record = &samples[1234];
        let enc = encode_with_dict(record, &dict).unwrap();
        let dec = decode_with_dict(&enc, &dict, record.len()).unwrap();
        assert_eq!(&dec, record);
    }

    #[test]
    fn dict_beats_plain_on_small_homogeneous_records() {
        let samples = homogeneous_samples(2000);
        let dict = train_dictionary(&samples, 8 * 1024).unwrap();

        // A single small record: plain zstd pays frame + can't reference the
        // shared structure; the dictionary already holds it.
        let record = &samples[42];
        let plain = encode_with_dict(record, &[]).unwrap();
        let with_dict = encode_with_dict(record, &dict).unwrap();
        assert!(
            with_dict.len() < plain.len(),
            "dict should win on small homogeneous data: dict={} plain={}",
            with_dict.len(),
            plain.len()
        );
    }
}
