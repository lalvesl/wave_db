//! High-level server entry point — the whole backend in one expression.
//!
//! An application's Quick-Node binary should not have to assemble a
//! [`Config`], open a listener, wire the router, and remember the
//! compaction/gossip side tasks.  That is this builder's job:
//!
//! ```rust,ignore
//! use wavedb_quick_node::Server;
//!
//! #[tokio::main]
//! async fn main() -> wavedb_core::Result<()> {
//!     Server::bind("127.0.0.1:7700")
//!         .tenant(42)
//!         .data_dir("./data")
//!         .registry(app_objects::REGISTRY)
//!         .serve()
//!         .await
//! }
//! ```
//!
//! Defaults: no peers, no Slow-Node, in-memory storage (no `data_dir`),
//! schema-blind (no `registry`).  Each `.tenant(n)` grants ownership of
//! that tenant's full shard range.  Anything beyond this surface (cluster
//! keys, custom shard ranges, …) drops down to [`Config`] +
//! [`QuickNode`] directly — the builder is the 90% path, not a wall.

use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::net::TcpListener;

use crate::config::{Config, OwnershipSpec};
use crate::node::QuickNode;
use crate::server::router;
use wavedb_core::ObjectRegistry;

/// Builder for a running Quick-Node server.  See the module docs.
#[derive(Debug)]
pub struct Server {
    listen: String,
    tenants: Vec<u64>,
    data_dir: PathBuf,
    registry: Option<&'static ObjectRegistry>,
    peers: String,
    slow_node: Option<String>,
}

impl Server {
    /// Start building a server that listens on `listen` (`host:port`;
    /// port `0` picks a free one — read it back from [`Running::addr`]).
    #[must_use]
    pub fn bind(listen: impl Into<String>) -> Self {
        Self {
            listen: listen.into(),
            tenants: Vec::new(),
            data_dir: PathBuf::new(),
            registry: None,
            peers: String::new(),
            slow_node: None,
        }
    }

    /// Own `tenant` (its full shard range).  Call once per tenant served
    /// by this node.
    #[must_use]
    pub fn tenant(mut self, tenant: u64) -> Self {
        self.tenants.push(tenant);
        self
    }

    /// Persist to `dir` (journal + data + heap files, WAL durability).
    /// Without this the node runs in-memory.
    #[must_use]
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = dir.into();
        self
    }

    /// Attach the application's static object registry
    /// (`declare_objects!`'s `REGISTRY`).  This is what turns the node
    /// into *your* backend: unknown headers refused, `validate` re-run,
    /// `preprocess` applied — all before the WAL commit.
    #[must_use]
    pub const fn registry(mut self, registry: &'static ObjectRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Comma-separated peer Quick-Node addresses to gossip with.
    #[must_use]
    pub fn peers(mut self, peers: impl Into<String>) -> Self {
        self.peers = peers.into();
        self
    }

    /// Slow-Node address to flush history to.
    #[must_use]
    pub fn slow_node(mut self, addr: impl Into<String>) -> Self {
        self.slow_node = Some(addr.into());
        self
    }

    /// Bind the listener and start serving in the background.
    ///
    /// Returns a [`Running`] handle exposing the bound address and the
    /// node; call [`Running::wait`] to park on the serve task.
    pub async fn start(self) -> wavedb_core::Result<Running> {
        if !self.data_dir.as_os_str().is_empty() {
            std::fs::create_dir_all(&self.data_dir)?;
        }
        let has_peers = !self.peers.is_empty();

        // Bind before building the Config so a `:0` listen address
        // resolves — the node then advertises the *real* port in its
        // Connect responses (owner/backup URLs).
        let listener = TcpListener::bind(&self.listen).await?;
        let addr = listener.local_addr()?;

        let config = Config {
            listen: addr.to_string(),
            peers: self.peers,
            slow_node: self.slow_node,
            owns: self
                .tenants
                .iter()
                .map(|&tenant| OwnershipSpec {
                    tenant,
                    shard_start: 0,
                    shard_end: 4095,
                })
                .collect(),
            bloom_interval_secs: 60,
            journal_compact_secs: 30,
            data_dir: self.data_dir,
            cluster_key: None,
        };

        let node = match self.registry {
            Some(reg) => QuickNode::with_registry(config, reg),
            None => QuickNode::new(config),
        };

        // Handle dropped on purpose: the loop self-terminates when the
        // node's inner state drops (it holds only a Weak ref).
        let _ = node.start_compaction_loop(30);
        if has_peers {
            let gossip = node.clone();
            tokio::spawn(async move { gossip.announce_self().await });
        }

        let serve_node = node.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router(serve_node)).await
        });

        Ok(Running { addr, node, task })
    }

    /// Bind and serve until the process is stopped.  The one-expression
    /// backend: `Server::bind(…).tenant(…).registry(…).serve().await`.
    pub async fn serve(self) -> wavedb_core::Result<()> {
        self.start().await?.wait().await
    }
}

/// A live, bound Quick-Node server returned by [`Server::start`].
#[derive(Debug)]
pub struct Running {
    addr: SocketAddr,
    node: QuickNode,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Running {
    /// The actually-bound socket address (resolves port `0`).
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The WebSocket URL clients pass to `Db::connect`.
    #[must_use]
    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    /// The underlying node handle (metrics, drain, storage).
    #[must_use]
    pub const fn node(&self) -> &QuickNode {
        &self.node
    }

    /// Park on the serve task until it exits.
    pub async fn wait(self) -> wavedb_core::Result<()> {
        match self.task.await {
            Ok(io) => io.map_err(wavedb_core::Error::Io),
            Err(join) => Err(wavedb_core::Error::Other(format!(
                "server task panicked: {join}"
            ))),
        }
    }
}
