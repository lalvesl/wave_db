//! Zstd compression for heap (variable-length) values.

/// Default zstd compression level for heap data.
const DEFAULT_LEVEL: i32 = 22; //3 is default

/// Compress bytes with zstd.
pub fn compress(data: &[u8]) -> crate::StorageResult<Vec<u8>> {
    Ok(zstd::encode_all(data, DEFAULT_LEVEL)?)
}

/// Compress with a custom level.
pub fn compress_level(
    data: &[u8],
    level: i32,
) -> crate::StorageResult<Vec<u8>> {
    Ok(zstd::encode_all(data, level)?)
}

/// Decompress zstd-compressed bytes.
pub fn decompress(data: &[u8]) -> crate::StorageResult<Vec<u8>> {
    Ok(zstd::decode_all(data)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small() {
        let data = b"hello world, this is a test of zstd compression";
        let compressed = compress(data).unwrap();
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(&decompressed, data);
    }

    #[test]
    fn roundtrip_large() {
        // 10KB of repetitive data — should compress well
        let data: Vec<u8> = (0u32..10_000)
            .map(|i| u8::try_from(i % 256).unwrap())
            .collect();
        let compressed = compress(&data).unwrap();
        assert!(
            compressed.len() < data.len(),
            "repetitive data should compress: {} -> {}",
            data.len(),
            compressed.len()
        );
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn roundtrip_empty() {
        let compressed = compress(b"").unwrap();
        let decompressed = decompress(&compressed).unwrap();
        assert!(decompressed.is_empty());
    }

    #[test]
    fn higher_level_better_ratio() {
        let data: Vec<u8> = (0u32..50_000)
            .map(|i| u8::try_from(i % 100).unwrap())
            .collect();
        let c1 = compress_level(&data, 1).unwrap();
        let c9 = compress_level(&data, 9).unwrap();
        // Higher level should produce equal or smaller output
        assert!(c9.len() <= c1.len());
    }
}
