//! Browser e2e: drive the exported example flow end to end.
//!
//! Run with:
//!   wasm-pack test --headless --firefox crates/wavedb-wasm
//! (or `--chrome`).  On native targets this file compiles to nothing.

#![cfg(target_arch = "wasm32")]
#![allow(clippy::future_not_send)]

use wasm_bindgen_test::*;
use wavedb_wasm::example_roundtrip;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
async fn e2e_example_roundtrip() {
    let report = example_roundtrip().await.expect("example flow failed");

    // Macro classified the variable-length field.
    assert_eq!(report.heapable_fields, vec!["title".to_string()]);

    // v1 bytes were upgraded to v2 through the migration chain.
    assert!(report.migrated_from_v1);
    assert!(!report.pinned_after_migration, "v2 default for new field");

    // Typed-column expression serialized to a non-empty request filter.
    assert!(report.expr_bytes > 0);

    // IndexedDB object store: put/get by 128-bit Id + tenant range scan.
    assert!(report.idb_roundtrip_ok);
    assert!(report.tenant_objects >= 1);
}
