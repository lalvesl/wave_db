//! Runtime configuration parameters for a WaveDB node.
//!
//! All parameters have documented defaults that match the README table.

use std::fmt;

/// Runtime configuration for a WaveDB node.
///
/// | Parameter                      | Default              |
/// |--------------------------------|----------------------|
/// | `page_size`                    | 4096 bytes (example) |
/// | `page_counter`                 | grows as needed      |
/// | `max_heap_inline`              | 25% of `page_size`   |
/// | `warning_size_page_occupation` | 70%                  |
/// | `max_dict_memory`              | 64 MiB               |
/// | `max_non_unique_elements`      | 50                   |
/// | `max_cached_size`              | 256 MiB              |
/// | `max_disk_iops`                | 1000                 |
/// | `lock_timeout_secs`            | 30 s                 |
/// | `bloom_filter_publish_interval`| 1 s                  |
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Bytes per page.
    pub page_size: usize,

    /// Largest heap value stored inline before overflow to a hashed block.
    /// Defaults to 25% of `page_size`.
    pub max_heap_inline: usize,

    /// Fill percentage at which a page emits a "nearly full" warning (0–100).
    pub warning_size_page_occupation: u8,

    /// RAM budget in bytes for per-STRUCT_ID dictionary cache.
    pub max_dict_memory: usize,

    /// Array → B+ tree conversion threshold for `NonUnique` collections.
    /// Can be overridden per-struct via the proc-macro attribute.
    pub max_non_unique_elements: u32,

    /// RAM budget in bytes for the in-memory write/read cache.
    /// Writes block when the cache approaches this limit.
    pub max_cached_size: usize,

    /// Soft IO budget per second across all background actors.
    pub max_disk_iops: u32,

    /// Maximum hold time in seconds for an ID-scoped lock.
    pub lock_timeout_secs: u64,

    /// How often (in seconds) nodes share their ownership Bloom filters.
    pub bloom_filter_publish_interval_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        let page_size = 4096 * 8; // 32 kB
        Self {
            page_size,
            max_heap_inline: page_size / 4,
            warning_size_page_occupation: 70,
            max_dict_memory: 64 * 1024 * 1024, // 64 MiB
            max_non_unique_elements: 50,
            max_cached_size: 256 * 1024 * 1024, // 256 MiB
            max_disk_iops: 1000,
            lock_timeout_secs: 30,
            bloom_filter_publish_interval_secs: 1,
        }
    }
}

impl Config {
    /// Create a development-friendly configuration backed by a single in-memory
    /// page buffer (small limits, no background IO budget).
    pub fn dev() -> Self {
        Self {
            page_size: 4096,
            max_heap_inline: 1024,
            warning_size_page_occupation: 90,
            max_dict_memory: 4 * 1024 * 1024,
            max_non_unique_elements: 50,
            max_cached_size: 16 * 1024 * 1024,
            max_disk_iops: 100,
            lock_timeout_secs: 5,
            bloom_filter_publish_interval_secs: 1,
        }
    }
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "WaveDB Config {{")?;
        writeln!(
            f,
            "  page_size:                    {} bytes",
            self.page_size
        )?;
        writeln!(
            f,
            "  max_heap_inline:              {} bytes",
            self.max_heap_inline
        )?;
        writeln!(
            f,
            "  warning_size_page_occupation: {}%",
            self.warning_size_page_occupation
        )?;
        writeln!(
            f,
            "  max_dict_memory:              {} bytes",
            self.max_dict_memory
        )?;
        writeln!(
            f,
            "  max_non_unique_elements:      {}",
            self.max_non_unique_elements
        )?;
        writeln!(
            f,
            "  max_cached_size:              {} bytes",
            self.max_cached_size
        )?;
        writeln!(f, "  max_disk_iops:                {}", self.max_disk_iops)?;
        writeln!(
            f,
            "  lock_timeout_secs:            {}s",
            self.lock_timeout_secs
        )?;
        writeln!(
            f,
            "  bloom_filter_publish_interval: {}s",
            self.bloom_filter_publish_interval_secs
        )?;
        write!(f, "}}")
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_heap_inline_is_quarter_page() {
        let cfg = Config::default();
        assert_eq!(cfg.max_heap_inline, cfg.page_size / 4);
    }

    #[test]
    fn dev_config_has_smaller_limits() {
        let dev = Config::dev();
        let prod = Config::default();
        assert!(dev.max_cached_size < prod.max_cached_size);
        assert!(dev.max_disk_iops < prod.max_disk_iops);
    }
}
