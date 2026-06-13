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
use wavedb_core::{Id, MigrationChain, Result, ValidationError, WaveDbStruct};
use wavedb_net::TransportResponse;
use wavedb_net::request::{
    ErrorCode, NodeError, RequestKind, TransportRequest,
};

use crate::object::{
    do_delete, do_query_non_unique, do_search_unique, do_write,
};
use crate::query::Expr;

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

/// Map a node's structured rejection back to the typed workspace error.
///
/// Validation and preprocess failures reconstruct the exact
/// [`wavedb_core::Error::Validation`] the hook produced node-side, so a
/// record that slipped past a stale client build fails with the same error
/// type the local pre-send check raises.
fn node_error_to_core(err: NodeError) -> wavedb_core::Error {
    match err.code {
        // For UnknownHeader the node packs the full header into `struct_id`.
        ErrorCode::UnknownHeader => {
            wavedb_core::Error::UnknownHeader(err.struct_id)
        }
        ErrorCode::ValidationFailed | ErrorCode::PreprocessFailed => {
            wavedb_core::Error::Validation {
                struct_id: err.struct_id,
                source: ValidationError {
                    field: err.field,
                    message: err.message,
                },
            }
        }
        ErrorCode::MalformedPayload
        | ErrorCode::Storage
        | ErrorCode::Unsupported => wavedb_core::Error::Other(format!(
            "node rejected request ({:?}): {}",
            err.code, err.message
        )),
    }
}

/// An erased async-send callable.
///
/// We cannot store `Box<dyn Transport>` because `Transport` has an async
/// method (which is not object-safe without the `async-trait` crate).
/// Instead we store a boxed closure that owns the concrete transport and
/// returns a pinned future.
type SendFn = Arc<
    dyn Fn(
            TransportRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<TransportResponse>>
                    + Send,
            >,
        > + Send
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use wavedb::Db;
    /// use wavedb_net::{MockTransport, mock::ScriptedReply};
    ///
    /// async fn example() -> wavedb_core::Result<()> {
    ///     let mock = MockTransport::new();
    ///     mock.push(ScriptedReply::connect("ws://owner:7700", "ws://backup:7700"));
    ///     let db = Db::open_with_transport(mock, 1, 100).await?;
    ///     assert_eq!(db.user(), 1);
    ///     assert_eq!(db.tenant(), 100);
    ///     Ok(())
    /// }
    /// ```
    pub async fn open_with_transport<T>(
        transport: T,
        user: u64,
        tenant: u64,
    ) -> Result<Self>
    where
        T: wavedb_net::Transport + Clone,
    {
        let connect_req =
            TransportRequest::new(0, RequestKind::Connect { user, tenant });
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

    async fn raw_send(
        &self,
        req: TransportRequest,
    ) -> Result<TransportResponse> {
        (self.send_fn)(req).await
    }

    /// Send a typed [`RequestKind`] and return the raw payload bytes.
    ///
    /// A structured [`NodeError`](wavedb_net::request::NodeError) in the
    /// response is mapped back to the matching typed [`wavedb_core::Error`]
    /// — a node-side `validate` rejection surfaces to application code as
    /// the **same** `Error::Validation` it would have gotten from the local
    /// pre-send check.
    pub async fn send(&self, kind: RequestKind) -> Result<Vec<u8>> {
        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed);
        let req = TransportRequest::new(seq, kind);
        let resp = self.raw_send(req).await?;
        if let Some(err) = resp.error {
            return Err(node_error_to_core(err));
        }
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

    // ── Generic CRUD sugar ───────────────────────────────────────────────
    //
    // These mirror `UniqueObject` / `NonUniqueObject` trait methods but read
    // more naturally at the call site:
    //
    //     db.save(&order).await?;
    //     let big = db.query::<Order>(amount.gt(100)).await?;
    //     let profile = db.find::<UserProfile>().await?;

    /// Look up the single live Unique record of type `T` for the current tenant.
    ///
    /// Equivalent to `T::search(&db)` but reads as a method call on `db`.
    pub async fn find<T>(&self) -> Result<Option<T>>
    where
        T: MigrationChain<Self> + WaveDbStruct,
    {
        do_search_unique::<T>(self).await
    }

    /// Query live NonUnique records of type `T` matching `expr`.
    ///
    /// Equivalent to `T::query(&db, expr)`.
    pub async fn query<T>(&self, expr: Expr) -> Result<Vec<T>>
    where
        T: MigrationChain<Self> + WaveDbStruct,
    {
        do_query_non_unique::<T>(self, expr).await
    }

    /// Persist `record` as a new version.
    ///
    /// Works for both Unique and NonUnique shapes.  Takes `&T` so the caller
    /// keeps ownership — no `.clone()` boilerplate at the call site.
    pub async fn save<T>(&self, record: &T) -> Result<()>
    where
        T: WaveDbStruct + wavedb_core::WaveDbHooks + wavedb_core::Wire + Sync,
    {
        do_write(self, record).await
    }

    /// Tombstone a NonUnique record by its raw [`Id`].
    pub async fn delete(&self, id: Id) -> Result<()> {
        do_delete(self, id.raw()).await
    }

    // ── URL-scheme constructor (native only) ────────────────────────────

    /// Open a session by URL scheme — the most ergonomic constructor.
    ///
    /// Picks a concrete transport based on the URL scheme:
    ///
    /// | Scheme | Transport |
    /// |--------|-----------|
    /// | `ws://`, `wss://` | [`WsClient`](wavedb_net::WsClient) (full-duplex, server-push) |
    /// | `http://`, `https://` | [`HttpClient`](wavedb_net::HttpClient) (single-queue POST) |
    ///
    /// The background receive / poll task is spawned on the current Tokio
    /// runtime.  It lives for as long as the returned `Db` (or any clone) is
    /// alive — `Db::drop` fires a `disconnect`, which lets the task exit.
    ///
    /// For custom transports or test harnesses, use
    /// [`Db::open_with_transport`] instead.
    ///
    /// # Errors
    ///
    /// - Unrecognised URL scheme → `Error::Transport`
    /// - Underlying transport handshake failure → bubbled from the client
    #[cfg(feature = "native")]
    pub async fn connect(url: &str, user: u64, tenant: u64) -> Result<Self> {
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("ws://") || lower.starts_with("wss://") {
            let (client, pump) =
                wavedb_net::WsClient::connect(url.to_string()).await?;
            tokio::spawn(pump);
            Self::open_with_transport(client, user, tenant).await
        } else if lower.starts_with("http://") || lower.starts_with("https://")
        {
            let client = wavedb_net::HttpClient::new(
                url.to_string(),
                std::time::Duration::from_millis(50),
            );
            let runner = client.clone();
            tokio::spawn(async move { runner.run().await });
            Self::open_with_transport(client, user, tenant).await
        } else {
            Err(wavedb_core::Error::Transport(format!(
                "unsupported URL scheme in `{url}` — expected ws://, wss://, http://, or https://"
            )))
        }
    }

    // ── Noop constructor ─────────────────────────────────────────────────

    /// Build a no-op `Db` with no real transport.
    ///
    /// Useful for unit tests that only exercise the query DSL or object
    /// serialisation without needing a network connection.
    ///
    /// # Examples
    ///
    /// ```
    /// use wavedb::Db;
    /// let db = Db::noop(1, 100);
    /// assert_eq!(db.user(), 1);
    /// assert_eq!(db.tenant(), 100);
    /// ```
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
                (self.inner.disconnect_hook)(
                    self.inner.user,
                    self.inner.tenant,
                );
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

        let send_fn: SendFn = Arc::new(|_req| {
            Box::pin(async {
                Err(wavedb_core::Error::Transport("noop".into()))
            })
        });

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
