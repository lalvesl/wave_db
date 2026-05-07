//! Page codec: encode/decode a page's stack data given its dictionary version.
//!
//! Each page header carries the dictionary version it was compressed with.
//! Old pages stay readable forever — the codec just needs the matching dict.

use crate::compression::heap_zstd;

/// Encode page stack data (with optional dictionary-assisted compression).
///
/// For now, uses plain zstd. When dictionaries are trained, the dict bytes
/// will be prepended to the zstd context for better ratios.
pub fn encode_stack(data: &[u8], _dict_version: u32) -> crate::StorageResult<Vec<u8>> {
    heap_zstd::compress(data)
}

/// Decode page stack data.
pub fn decode_stack(compressed: &[u8], _dict_version: u32) -> crate::StorageResult<Vec<u8>> {
    heap_zstd::decompress(compressed)
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
}
