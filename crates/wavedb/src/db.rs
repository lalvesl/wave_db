//! The [`Db`] connection handle — the entry point to all `WaveDB` operations.
//!
//! `Db` is intentionally cheap to clone: it's a reference-counted handle to
//! shared inner state.  Cloning does **not** open a new session; it shares
//! the existing connection.  If you need a second tenant, use
//! [`Db::another_tenant`].

use parking_lot::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use wavedb_core::{Id, Result};
use wavedb_net::TransportResponse;
use wavedb_net::request::{RequestKind, TransportRequest};

// ── Disconnect sink ─────────────────────────────────────────────────────────
//
// We need Drop to fire a disconnect on the transport, but Rust doesn't allow
// a conditional `Drop` implementation (you can't write `impl<T: Transport> Drop
// for Db<T>` when the struct has a default `T = ()`).
//
// Solution: store an erased disconnect hook as a boxed closure.  The concrete
// transport is captured inside the closure; `Db` itself remains a plain
// struct with no generic parameter.

type DisconnectHook = Box<dyn Fn(u64, u64) + Send + Sync + 'static>;

/// An erased async-send callable.
///
/// We cannot store `Box<dyn Transport>` because `Transport` has an async
/// method (which is not object-safe without the `async-trait` crate).
/// Instead we store a boxed closure that owns the concrete transport and
/// returns a pinned future.
type SendFn = Arc<
    dyn Fn(
            TransportRequest,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TransportResponse>> + Send>>
        + Send
        + Sync
        + 'static,
>;

/// Shared, reference-counted state inside a [`Db`] handle.
struct DbInner {
    /// The authenticated user.
    user: u64,
    /// The active tenant.
    tenant: u64,
    /// The Quick-Node URL that currently owns this tenant's partition.
    owner_url: Mutex<String>,
    /// A backup Quick-Node URL used for failover.
    backup_url: Mutex<String>,
    /// Monotonically increasing per-session sequence counter.
    seq: AtomicU64,
    /// Called when the last `Db` clone drops.  May be a no-op.
    disconnect_hook: DisconnectHook,
    /// Whether `Drop` has already fired a disconnect (avoids double-send).
    disconnected: Mutex<bool>,
}

impl std::fmt::Debug for DbInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbInner")
            .field("user", &self.user)
            .field("tenant", &self.tenant)
            .field("owner_url", &*self.owner_url.lock())
            .finish_non_exhaustive()
    }
}

/// The primary `WaveDB` connection handle.
///
/// `Db` is non-generic and cheap to clone (it's `Arc`-backed).
///
/// # Object lifecycle
///
/// All record types implement either [`UniqueObject`](crate::object::UniqueObject)
/// or [`NonUniqueObject`](crate::object::NonUniqueObject).  Use those traits
/// to create, read, update, and delete records.
///
/// # Session teardown
///
/// `Db` implements `Drop`: when the last clone drops, the Quick-Node receives a
/// disconnect message so it can release the session slot promptly.
#[derive(Clone)]
pub struct Db {
    inner: Arc<DbInner>,
    /// Erased send function — calls the underlying transport.
    send_fn: SendFn,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("inner", &self.inner)
            .field("send_fn", &"<erased>")
            .finish()
    }
}

// ── Constructors ────────────────────────────────────────────────────────────

impl Db {
    /// Open a session using an explicit [`Transport`](wavedb_net::Transport)
    /// implementation.
    ///
    /// This is the low-level constructor used in tests and for custom
    /// transports.  The transport is expected to answer the first request —
    /// a `Connect` request — with `owner_url` and `backup_url` fields set
    /// in the response.
    pub async fn open_with_transport<T>(transport: T, user: u64, tenant: u64) -> Result<Self>
    where
        T: wavedb_net::Transport + Clone,
    {
        let connect_req = TransportRequest::new(0, RequestKind::Connect { user, tenant });
        let resp = transport.send(connect_req).await?;

        let owner_url = resp
            .owner_url
            .unwrap_or_else(|| String::from("ws://localhost:7700"));
        let backup_url = resp
            .backup_url
            .unwrap_or_else(|| String::from("ws://localhost:7701"));

        // Erase the transport type behind a closure.
        let send_transport = transport.clone();
        let send_fn: SendFn = Arc::new(move |req| {
            let t = send_transport.clone();
            Box::pin(async move { t.send(req).await })
        });

        // Erase the disconnect hook similarly.
        let disc_transport = transport;
        let disconnect_hook: DisconnectHook =
            Box::new(move |u, ten| disc_transport.disconnect(u, ten));

        Ok(Self {
            inner: Arc::new(DbInner {
                user,
                tenant,
                owner_url: Mutex::new(owner_url),
                backup_url: Mutex::new(backup_url),
                seq: AtomicU64::new(1),
                disconnect_hook,
                disconnected: Mutex::new(false),
            }),
            send_fn,
        })
    }

    /// Spawn a new `Db` handle targeting a different tenant, sharing the
    /// same underlying transport.
    pub async fn another_tenant(&self, tenant: u64) -> Result<Self> {
        let next_seq = self.inner.seq.fetch_add(1, Ordering::Relaxed);
        let connect_req = TransportRequest::new(
            next_seq,
            RequestKind::Connect {
                user: self.inner.user,
                tenant,
            },
        );
        let resp = self.raw_send(connect_req).await?;

        let owner_url = resp
            .owner_url
            .unwrap_or_else(|| self.inner.owner_url.lock().clone());
        let backup_url = resp
            .backup_url
            .unwrap_or_else(|| self.inner.backup_url.lock().clone());

        // Share the same send_fn (same physical transport) but bind a new inner
        // with the new tenant.  The disconnect hook is a no-op: the parent session
        // is responsible for disconnecting the shared transport.
        let new_send_fn = Arc::clone(&self.send_fn);

        Ok(Self {
            inner: Arc::new(DbInner {
                user: self.inner.user,
                tenant,
                owner_url: Mutex::new(owner_url),
                backup_url: Mutex::new(backup_url),
                seq: AtomicU64::new(next_seq + 1),
                disconnect_hook: Box::new(|_, _| {}),
                disconnected: Mutex::new(false),
            }),
            send_fn: new_send_fn,
        })
    }

    // ── Low-level send ───────────────────────────────────────────────────

    async fn raw_send(&self, req: TransportRequest) -> Result<TransportResponse> {
        (self.send_fn)(req).await
    }

    /// Send a typed [`RequestKind`] and return the raw payload bytes.
    pub async fn send(&self, kind: RequestKind) -> Result<Vec<u8>> {
        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed);
        let req = TransportRequest::new(seq, kind);
        let resp = self.raw_send(req).await?;
        Ok(resp.payload)
    }

    // ── Accessors ────────────────────────────────────────────────────────

    /// The authenticated user for this session.
    pub fn user(&self) -> u64 {
        self.inner.user
    }

    /// The active tenant for this session.
    pub fn tenant(&self) -> u64 {
        self.inner.tenant
    }

    /// The URL of the Quick-Node that currently owns this session's tenant.
    pub fn owner_url(&self) -> String {
        self.inner.owner_url.lock().clone()
    }

    /// The URL of the backup Quick-Node for failover.
    pub fn backup_url(&self) -> String {
        self.inner.backup_url.lock().clone()
    }

    /// Resolve the Unique anchor key for the given struct under the current tenant.
    ///
    /// For Unique objects, `shard_id = 0` and `created_at = 0`.
    pub fn unique_anchor_id(&self, struct_id: u32) -> Id {
        Id::new(self.inner.tenant, 0, struct_id, 0)
    }

    // ── Noop constructor ─────────────────────────────────────────────────

    /// Build a no-op `Db` with no real transport.
    ///
    /// Useful for unit tests that only exercise the query DSL or object
    /// serialisation without needing a network connection.
    pub fn noop(user: u64, tenant: u64) -> Self {
        let send_fn: SendFn = Arc::new(|_req| {
            Box::pin(async {
                Err(wavedb_core::Error::Transport(
                    "noop Db cannot send requests".into(),
                ))
            })
        });

        Self {
            inner: Arc::new(DbInner {
                user,
                tenant,
                owner_url: Mutex::new(String::new()),
                backup_url: Mutex::new(String::new()),
                seq: AtomicU64::new(0),
                disconnect_hook: Box::new(|_, _| {}),
                disconnected: Mutex::new(false),
            }),
            send_fn,
        }
    }
}

// ── Drop ────────────────────────────────────────────────────────────────────

impl Drop for Db {
    fn drop(&mut self) {
        // Only the last live clone fires the disconnect.
        if Arc::strong_count(&self.inner) == 1 {
            let should_disconnect = {
                let mut disconnected = self.inner.disconnected.lock();
                if *disconnected {
                    false
                } else {
                    *disconnected = true;
                    true
                }
            };
            if should_disconnect {
                (self.inner.disconnect_hook)(self.inner.user, self.inner.tenant);
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_db_basic_accessors() {
        let db = Db::noop(1, 100);
        assert_eq!(db.user(), 1);
        assert_eq!(db.tenant(), 100);
    }

    #[test]
    fn clone_shares_inner() {
        let db = Db::noop(1, 100);
        let db2 = db.clone();
        assert!(Arc::ptr_eq(&db.inner, &db2.inner));
    }

    #[test]
    fn drop_fires_disconnect_exactly_once() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering as AOrdering};

        let counter = StdArc::new(AtomicUsize::new(0));
        let c = StdArc::clone(&counter);

        let send_fn: SendFn =
            Arc::new(|_req| Box::pin(async { Err(wavedb_core::Error::Transport("noop".into())) }));

        let db = Db {
            inner: Arc::new(DbInner {
                user: 1,
                tenant: 100,
                owner_url: Mutex::new(String::new()),
                backup_url: Mutex::new(String::new()),
                seq: AtomicU64::new(0),
                disconnect_hook: Box::new(move |_, _| {
                    c.fetch_add(1, AOrdering::SeqCst);
                }),
                disconnected: Mutex::new(false),
            }),
            send_fn,
        };

        let db2 = db.clone();
        drop(db);
        // db2 still alive — hook should not have fired yet.
        assert_eq!(counter.load(AOrdering::SeqCst), 0);
        drop(db2);
        // Now the last clone dropped — hook fires exactly once.
        assert_eq!(counter.load(AOrdering::SeqCst), 1);
    }

    #[test]
    fn unique_anchor_id_shape() {
        let db = Db::noop(42, 100);
        let anchor = db.unique_anchor_id(7);
        assert_eq!(anchor.tenant_id(), 100);
        assert_eq!(anchor.shard_id(), 0);
        assert_eq!(anchor.struct_id(), 7);
        assert_eq!(anchor.created_at(), 0);
    }
}
