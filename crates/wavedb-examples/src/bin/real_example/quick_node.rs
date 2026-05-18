//! Quick-node orchestration helpers: topology display and failure injection.
//!
//! A quick-node "failure" in this scenario is a *graceful drain*: the operator
//! calls `node.drain()` which atomically sets the `draining` flag.  All write
//! handlers immediately start returning `b"draining"` responses, causing every
//! connected client to detect the failure and stop sending.  The Withdraw gossip
//! is fanned out to surviving peers so they remove the draining node from their
//! consistent-hash rings.

use wavedb_monitor::{LogBuffer, push_log};
use wavedb_test_cluster::TestCluster;

/// Print quick-node addresses and connectivity info.
pub fn print_topology(cluster: &TestCluster, log: &LogBuffer) {
    for (i, node) in cluster.quick_nodes.iter().enumerate() {
        push_log(
            log,
            format!(
                "  quick-node[{i}]  {}, data_dir: {}",
                node.ws_url(),
                node.storage()
                    .map_or("<none>", |s| s.data_dir.to_str().unwrap_or_default())
            ),
        );
    }
}
