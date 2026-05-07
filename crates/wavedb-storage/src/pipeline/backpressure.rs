//! Write backpressure: blocks incoming writes when cache approaches capacity.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Backpressure controller for the write pipeline.
///
/// When the cache approaches `max_cached_size`, incoming writes block
/// until the drain actor frees enough space.
pub struct Backpressure {
    /// Current cache usage in bytes.
    current_usage: AtomicUsize,
    /// Maximum allowed cache size in bytes.
    max_size: usize,
    /// Warning threshold (fraction of max_size).
    warn_threshold: f64,
}

#[allow(clippy::cast_precision_loss)]
impl Backpressure {
    /// Create a new backpressure controller.
    pub const fn new(max_size: usize) -> Self {
        Self {
            current_usage: AtomicUsize::new(0),
            max_size,
            warn_threshold: 0.8,
        }
    }

    /// Record that `bytes` were added to the cache.
    pub fn record_write(&self, bytes: usize) {
        self.current_usage.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record that `bytes` were drained from the cache.
    pub fn record_drain(&self, bytes: usize) {
        self.current_usage
            .fetch_sub(bytes.min(self.current()), Ordering::Relaxed);
    }

    /// Check whether writes should be blocked.
    pub fn should_block(&self) -> bool {
        self.current() >= self.max_size
    }

    /// Check whether we're in the warning zone.
    pub fn is_warning(&self) -> bool {
        self.current() as f64 >= self.max_size as f64 * self.warn_threshold
    }

    /// Current cache usage in bytes.
    pub fn current(&self) -> usize {
        self.current_usage.load(Ordering::Relaxed)
    }

    /// Maximum cache size.
    pub const fn max_size(&self) -> usize {
        self.max_size
    }

    /// Usage as a fraction (0.0 to 1.0+).
    pub fn usage_ratio(&self) -> f64 {
        self.current() as f64 / self.max_size as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_flow() {
        let bp = Backpressure::new(1000);
        assert!(!bp.should_block());
        assert!(!bp.is_warning());
        assert_eq!(bp.current(), 0);

        bp.record_write(500);
        assert!(!bp.should_block());
        assert!(!bp.is_warning());

        bp.record_write(400);
        assert!(!bp.should_block());
        assert!(bp.is_warning()); // 900/1000 = 90% > 80%

        bp.record_write(200);
        assert!(bp.should_block()); // 1100 > 1000

        bp.record_drain(500);
        assert!(!bp.should_block()); // 600 < 1000
    }

    #[test]
    fn usage_ratio() {
        let bp = Backpressure::new(1000);
        bp.record_write(250);
        let ratio = bp.usage_ratio();
        assert!((ratio - 0.25).abs() < f64::EPSILON);
    }
}
