//! Minimal async IndexedDB key-value store for the WASM client.
//!
//! This is the browser-side persistence layer: records cached by the
//! write-through layer and small bookkeeping entries (like the owner URL)
//! all go through one object store.  localStorage is unsuitable for this
//! role — synchronous API, string-only values, ~5 MB quota — so IndexedDB
//! is the only backend.
//!
//! **No page emulation in the browser.**  IndexedDB is already an ordered
//! key→value store doing its own page management internally, so WaveDB
//! stores one key per object — anchors at their anchor key, versioned
//! records at their full 128-bit Id, heap-dedup entries at their content
//! hash — instead of stacking a second block manager on top.  The
//! page-pressure machinery (heapable eviction, double hashing, rebalance)
//! never runs client-side.  See the readme section
//! _Browser storage: key→value, not pages_.

use std::cell::RefCell;
use std::rc::Rc;

use futures::channel::oneshot;
use js_sys::Uint8Array;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{IdbDatabase, IdbOpenDbRequest, IdbRequest, IdbTransactionMode};

/// Default IndexedDB database name used by the WaveDB WASM client.
pub const DB_NAME: &str = "wavedb";

/// Object store holding cached pages and client bookkeeping entries.
pub const STORE_NAME: &str = "kv";

/// Key under which the last successful owner URL is persisted.
pub const OWNER_URL_KEY: &str = "wavedb_owner_url";

/// Async handle to one IndexedDB object store.
///
/// Open with [`IdbStore::open`] (the WaveDB default database) or
/// [`IdbStore::open_named`]; then `get` / `put` / `delete` byte values by
/// string key.  All methods are usable both from Rust and, via
/// `wasm-bindgen`, from JavaScript.
#[wasm_bindgen]
pub struct IdbStore {
    db: IdbDatabase,
    store: String,
}

#[wasm_bindgen]
impl IdbStore {
    /// Open (creating on first use) the default WaveDB database.
    pub async fn open() -> Result<Self, JsValue> {
        Self::open_named(DB_NAME, STORE_NAME).await
    }

    /// Open (creating on first use) a specific database / object store pair.
    pub async fn open_named(
        db_name: &str,
        store_name: &str,
    ) -> Result<Self, JsValue> {
        let factory = web_sys::window()
            .ok_or_else(|| JsValue::from_str("no window object"))?
            .indexed_db()?
            .ok_or_else(|| JsValue::from_str("IndexedDB unavailable"))?;

        let open_req: IdbOpenDbRequest = factory.open_with_u32(db_name, 1)?;

        // First open (or version bump): create the object store.
        let on_upgrade = {
            let store_name = store_name.to_owned();
            let req = open_req.clone();
            Closure::wrap(Box::new(move |_: web_sys::Event| {
                let Ok(result) = req.result() else { return };
                let Ok(db) = result.dyn_into::<IdbDatabase>() else {
                    return;
                };
                if !db.object_store_names().contains(&store_name) {
                    let _ = db.create_object_store(&store_name);
                }
            }) as Box<dyn FnMut(_)>)
        };
        open_req.set_onupgradeneeded(Some(on_upgrade.as_ref().unchecked_ref()));

        let db = await_request(open_req.as_ref())
            .await?
            .dyn_into::<IdbDatabase>()?;
        drop(on_upgrade);

        Ok(Self {
            db,
            store: store_name.to_owned(),
        })
    }

    /// Read the value stored under `key`, or `None` if absent.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, JsValue> {
        let tx = self.db.transaction_with_str(&self.store)?;
        let store = tx.object_store(&self.store)?;
        let req = store.get(&JsValue::from_str(key))?;
        let result = await_request(&req).await?;
        if result.is_undefined() || result.is_null() {
            return Ok(None);
        }
        Ok(Some(Uint8Array::new(&result).to_vec()))
    }

    /// Write `value` under `key`, overwriting any previous value.
    pub async fn put(&self, key: &str, value: &[u8]) -> Result<(), JsValue> {
        let tx = self.db.transaction_with_str_and_mode(
            &self.store,
            IdbTransactionMode::Readwrite,
        )?;
        let store = tx.object_store(&self.store)?;
        let bytes = Uint8Array::from(value);
        let req =
            store.put_with_key(bytes.as_ref(), &JsValue::from_str(key))?;
        await_request(&req).await?;
        Ok(())
    }

    /// Delete the value stored under `key` (no-op if absent).
    pub async fn delete(&self, key: &str) -> Result<(), JsValue> {
        let tx = self.db.transaction_with_str_and_mode(
            &self.store,
            IdbTransactionMode::Readwrite,
        )?;
        let store = tx.object_store(&self.store)?;
        let req = store.delete(&JsValue::from_str(key))?;
        await_request(&req).await?;
        Ok(())
    }
}

/// Await an `IdbRequest`, resolving with its `result()` on success.
async fn await_request(req: &IdbRequest) -> Result<JsValue, JsValue> {
    let (tx, rx) = oneshot::channel::<Result<JsValue, JsValue>>();
    let tx = Rc::new(RefCell::new(Some(tx)));

    let on_success = {
        let tx = Rc::clone(&tx);
        let req = req.clone();
        Closure::wrap(Box::new(move |_: web_sys::Event| {
            if let Some(s) = tx.borrow_mut().take() {
                let _ = s.send(req.result());
            }
        }) as Box<dyn FnMut(_)>)
    };
    let on_error = {
        let tx = Rc::clone(&tx);
        let req = req.clone();
        Closure::wrap(Box::new(move |_: web_sys::Event| {
            if let Some(s) = tx.borrow_mut().take() {
                let err = req.error().ok().flatten().map_or_else(
                    || JsValue::from_str("IndexedDB request failed"),
                    JsValue::from,
                );
                let _ = s.send(Err(err));
            }
        }) as Box<dyn FnMut(_)>)
    };
    req.set_onsuccess(Some(on_success.as_ref().unchecked_ref()));
    req.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    let out = rx
        .await
        .map_err(|_| JsValue::from_str("IndexedDB channel dropped"))?;
    // Closures must outlive the await — drop only after the event fired.
    drop(on_success);
    drop(on_error);
    out
}

// ── wasm-bindgen-test tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn put_get_delete_roundtrip() {
        let store = IdbStore::open_named("wavedb-test", "kv").await.unwrap();
        store.put("k", b"v1").await.unwrap();
        assert_eq!(store.get("k").await.unwrap().as_deref(), Some(&b"v1"[..]));
        store.put("k", b"v2").await.unwrap();
        assert_eq!(store.get("k").await.unwrap().as_deref(), Some(&b"v2"[..]));
        store.delete("k").await.unwrap();
        assert!(store.get("k").await.unwrap().is_none());
    }

    #[wasm_bindgen_test]
    async fn get_missing_key_is_none() {
        let store = IdbStore::open_named("wavedb-test", "kv").await.unwrap();
        assert!(store.get("never-written").await.unwrap().is_none());
    }
}
