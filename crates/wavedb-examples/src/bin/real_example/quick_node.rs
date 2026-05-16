//! Quick-node orchestration helpers: topology display and failure injection.
//!
//! A quick-node "failure" in this scenario is a *graceful drain*: the operator
//! calls `node.drain()` which atomically sets the `draining` flag.  All write
//! handlers immediately start returning `b"draining"` responses, causing every
//! connected client to detect the failure and stop sending.  The Withdraw gossip
//! is fanned out to surviving peers so they remove the draining node from their
//! consistent-hash rings.

use wavedb_test_cluster::TestCluster;

/// Print quick-node addresses and connectivity info.
pub fn print_topology(cluster: &TestCluster) {
    for (i, node) in cluster.quick_nodes.iter().enumerate() {
        println!("  quick-node[{i}]  {}", node.ws_url());
    }
}
