//! Object-changed notification bus with bloom-filter deduplication.
//!
//! When a Quick-Node sends piggyback notifications inside an HTTP response or
//! pushes anchor-update frames over WebSocket, they flow through an
//! [`EventBus`].  Subscribers receive a stream of [`AnchorEvent`]s.
//!
//! # Bloom-filter sync
//!
//! Each client maintains a [`BloomFilter`] of the anchor IDs it has already
//! seen.  On reconnect the client ships this filter to the Quick-Node, which
//! then only pushes IDs **not** in the filter — avoiding a thundering-herd of
//! duplicate events after a brief network hiccup.

use fastbloom::BloomFilter;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::broadcast;

/// An object-changed notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorEvent {
    /// The raw 128-bit anchor ID that changed.
    pub anchor_id: u128,
    /// `true` if the record was deleted (tombstoned).
    pub deleted: bool,
    /// Optional inline payload for the new live data (small anchors).
    pub payload: Option<Vec<u8>>,
}

impl AnchorEvent {
    /// Create a live-data event.
    pub const fn updated(anchor_id: u128, payload: Option<Vec<u8>>) -> Self {
        Self {
            anchor_id,
            deleted: false,
            payload,
        }
    }

    /// Create a tombstone event.
    pub const fn deleted(anchor_id: u128) -> Self {
        Self {
            anchor_id,
            deleted: true,
            payload: None,
        }
    }
}

// ── EventBus ─────────────────────────────────────────────────────────────────

/// Channel capacity for the broadcast bus.
const BUS_CAPACITY: usize = 512;

/// A publish-subscribe bus for [`AnchorEvent`]s.
///
/// Publishers call [`EventBus::publish`]; subscribers obtain a
/// [`tokio::sync::broadcast::Receiver`] via [`EventBus::subscribe`].
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<AnchorEvent>,
}

impl EventBus {
    /// Create a new event bus with the default channel capacity.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self { tx }
    }

    /// Publish an event to all current subscribers.
    ///
    /// Returns the number of receivers that received the event.
    /// A return value of `0` is normal when there are no subscribers yet.
    pub fn publish(&self, event: AnchorEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Subscribe to future events on this bus.
    pub fn subscribe(&self) -> broadcast::Receiver<AnchorEvent> {
        self.tx.subscribe()
    }

    /// Number of active subscribers.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

// ── BloomFilter sync ─────────────────────────────────────────────────────────

/// Number of items the bloom filter is sized for.
const BLOOM_CAPACITY: usize = 100_000;
/// Target false-positive rate.
const BLOOM_FP_RATE: f64 = 0.01;

/// A thread-safe client-side bloom filter tracking seen anchor IDs.
///
/// On reconnect the client serialises this filter and sends it to the server
/// so the server can skip duplicate push notifications.
#[derive(Debug, Clone)]
pub struct SeenFilter {
    inner: Arc<Mutex<BloomFilter>>,
}

impl SeenFilter {
    /// Create a new empty seen-filter.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(
                BloomFilter::with_false_pos(BLOOM_FP_RATE)
                    .expected_items(BLOOM_CAPACITY),
            )),
        }
    }

    /// Record that `anchor_id` has been seen.
    pub fn mark(&self, anchor_id: u128) {
        self.inner.lock().insert(&anchor_id);
    }

    /// Returns `true` if `anchor_id` is **probably** already seen.
    pub fn probably_seen(&self, anchor_id: u128) -> bool {
        self.inner.lock().contains(&anchor_id)
    }

    /// Serialise the filter to bytes for transmission to the server.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.inner
            .lock()
            .as_slice()
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect()
    }
}

impl Default for SeenFilter {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_bus_publish_subscribe() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        let event = AnchorEvent::updated(42, Some(vec![1, 2, 3]));
        let sent = bus.publish(event);
        assert_eq!(sent, 1);

        let received = rx.try_recv().unwrap();
        assert_eq!(received.anchor_id, 42);
        assert!(!received.deleted);
        assert_eq!(received.payload, Some(vec![1, 2, 3]));
    }

    #[test]
    fn tombstone_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.publish(AnchorEvent::deleted(99));
        let e = rx.try_recv().unwrap();
        assert!(e.deleted);
        assert_eq!(e.anchor_id, 99);
    }

    #[test]
    fn publish_with_no_subscribers_is_ok() {
        let bus = EventBus::new();
        let n = bus.publish(AnchorEvent::updated(1, None));
        assert_eq!(n, 0); // no subscribers — that's fine
    }

    #[test]
    fn seen_filter_mark_and_check() {
        let f = SeenFilter::new();
        assert!(!f.probably_seen(1234));
        f.mark(1234);
        assert!(f.probably_seen(1234));
    }

    #[test]
    fn seen_filter_serialises_to_bytes() {
        let f = SeenFilter::new();
        f.mark(100);
        f.mark(200);
        let bytes = f.to_bytes();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn multiple_subscribers_all_receive() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish(AnchorEvent::updated(7, None));

        assert_eq!(rx1.try_recv().unwrap().anchor_id, 7);
        assert_eq!(rx2.try_recv().unwrap().anchor_id, 7);
    }
}
