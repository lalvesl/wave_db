//! `WaveDB` WASM client (Phase 15).
//!
//! All code is gated on `target_arch = "wasm32"`.  On native targets this
//! crate is empty — it only exists so `cargo test --workspace` can compile it.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::future_not_send)]

// ── WASM implementation ───────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
mod wasm_impl {
    use gloo_storage::{LocalStorage, Storage};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    use futures::channel::oneshot;
    use js_sys::{ArrayBuffer, Uint8Array};
    use wasm_bindgen::JsCast;
    use wasm_bindgen::prelude::*;

    use wavedb_net::frame::{decode_payload, encode_payload};
    use wavedb_net::request::{RequestKind, TransportRequest, TransportResponse};

    type PendingMap = Rc<RefCell<HashMap<u64, oneshot::Sender<TransportResponse>>>>;

    /// JS-visible handle to a WaveDB Quick-Node session.
    ///
    /// Open with [`JsDb::connect`]; send requests with [`JsDb::write`],
    /// [`JsDb::search_unique`], etc.; call [`JsDb::disconnect`] to close.
    #[wasm_bindgen]
    pub struct JsDb {
        ws: web_sys::WebSocket,
        pending: PendingMap,
        seq: Cell<u64>,
        user: u64,
        tenant: u64,
        owner_url: String,
        backup_url: String,
        // Keep the onmessage closure alive for the lifetime of JsDb.
        _onmessage: Closure<dyn FnMut(web_sys::MessageEvent)>,
    }

    #[wasm_bindgen]
    impl JsDb {
        /// Open a session to the Quick-Node at `ws_url` for `user`/`tenant`.
        ///
        /// Waits for the underlying WebSocket to open and sends the initial
        /// `Connect` request before resolving.
        pub async fn connect(ws_url: &str, user: u64, tenant: u64) -> Result<Self, JsValue> {
            let ws = web_sys::WebSocket::new(ws_url)?;
            ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

            // ── Wait for onopen or onerror ────────────────────────────────────
            let (open_tx, open_rx) = oneshot::channel::<Result<(), JsValue>>();
            let open_tx = Rc::new(RefCell::new(Some(open_tx)));

            {
                let tx = Rc::clone(&open_tx);
                let cb = Closure::wrap(Box::new(move |_: web_sys::Event| {
                    if let Some(s) = tx.borrow_mut().take() {
                        let _ = s.send(Ok(()));
                    }
                }) as Box<dyn FnMut(_)>);
                ws.set_onopen(Some(cb.as_ref().unchecked_ref()));
                cb.forget();
            }

            {
                let tx = Rc::clone(&open_tx);
                let cb = Closure::wrap(Box::new(move |_: web_sys::Event| {
                    if let Some(s) = tx.borrow_mut().take() {
                        let _ = s.send(Err(JsValue::from_str("WebSocket connection failed")));
                    }
                }) as Box<dyn FnMut(_)>);
                ws.set_onerror(Some(cb.as_ref().unchecked_ref()));
                cb.forget();
            }

            open_rx
                .await
                .map_err(|_| JsValue::from_str("open-channel dropped"))??;

            // ── Install persistent onmessage handler ──────────────────────────
            let pending: PendingMap = Rc::new(RefCell::new(HashMap::new()));
            let pending_msg = Rc::clone(&pending);

            let onmessage = Closure::wrap(Box::new(move |evt: web_sys::MessageEvent| {
                let Ok(ab) = evt.data().dyn_into::<ArrayBuffer>() else {
                    return;
                };
                let bytes: Vec<u8> = Uint8Array::new(&ab).to_vec();
                let Ok(resp) = decode_payload::<TransportResponse>(&bytes) else {
                    return;
                };
                if let Some(tx) = pending_msg.borrow_mut().remove(&resp.seq) {
                    let _ = tx.send(resp);
                }
            }) as Box<dyn FnMut(_)>);
            ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

            // ── Connect handshake ─────────────────────────────────────────────
            let seq = 0u64;
            let (tx, rx) = oneshot::channel::<TransportResponse>();
            pending.borrow_mut().insert(seq, tx);

            let req = TransportRequest::new(seq, RequestKind::Connect { user, tenant });
            let bytes = encode_payload(&req).map_err(|e| JsValue::from_str(&e.to_string()))?;
            ws.send_with_u8_array(&bytes)?;

            let resp = rx
                .await
                .map_err(|_| JsValue::from_str("disconnected during Connect handshake"))?;

            let owner_url = resp.owner_url.unwrap_or_default();
            let backup_url = resp.backup_url.unwrap_or_default();

            // Persist owner URL in localStorage so clients can reconnect after
            // a page refresh without re-discovering the owner via the ring.
            LocalStorage::set("wavedb_owner_url", &owner_url).ok();

            Ok(Self {
                ws,
                pending,
                seq: Cell::new(1),
                user,
                tenant,
                owner_url,
                backup_url,
                _onmessage: onmessage,
            })
        }

        /// The Quick-Node URL that owns this session's partition.
        pub fn owner_url(&self) -> String {
            self.owner_url.clone()
        }

        /// The backup Quick-Node URL for failover.
        pub fn backup_url(&self) -> String {
            self.backup_url.clone()
        }

        /// The authenticated user for this session.
        #[allow(clippy::missing_const_for_fn)]
        pub fn user(&self) -> u64 {
            self.user
        }

        /// The active tenant for this session.
        #[allow(clippy::missing_const_for_fn)]
        pub fn tenant(&self) -> u64 {
            self.tenant
        }

        /// Write (create or update) a record.
        ///
        /// `payload` must be a postcard-encoded struct byte slice.
        /// Returns the raw response payload, or an error if the write was
        /// rejected (e.g. `b"not_owner"`).
        pub async fn write(&self, struct_id: u32, payload: Vec<u8>) -> Result<Vec<u8>, JsValue> {
            let resp = self
                .send_raw(RequestKind::Write {
                    struct_id,
                    user: self.user,
                    tenant: self.tenant,
                    payload,
                })
                .await?;
            Ok(resp.payload)
        }

        /// Search for a Unique record.
        ///
        /// Returns raw postcard-encoded bytes, or empty if no record exists yet.
        pub async fn search_unique(&self, struct_id: u32) -> Result<Vec<u8>, JsValue> {
            let resp = self
                .send_raw(RequestKind::SearchUnique {
                    struct_id,
                    user: self.user,
                    tenant: self.tenant,
                })
                .await?;
            Ok(resp.payload)
        }

        /// Query NonUnique records.
        ///
        /// `filter` is a postcard-encoded `Expr` byte slice; pass an empty
        /// slice to return all records.
        pub async fn query_non_unique(
            &self,
            struct_id: u32,
            filter: Vec<u8>,
        ) -> Result<Vec<u8>, JsValue> {
            let resp = self
                .send_raw(RequestKind::QueryNonUnique {
                    struct_id,
                    user: self.user,
                    tenant: self.tenant,
                    filter,
                })
                .await?;
            Ok(resp.payload)
        }

        /// Delete a record (write a tombstone).
        pub async fn delete(&self, id: u128) -> Result<(), JsValue> {
            self.send_raw(RequestKind::Delete {
                id,
                user: self.user,
                tenant: self.tenant,
            })
            .await?;
            Ok(())
        }

        /// Disconnect cleanly: send a `Disconnect` request then close the socket.
        pub fn disconnect(&self) {
            let seq = self.seq.get();
            if let Ok(bytes) = encode_payload(&TransportRequest::new(
                seq,
                RequestKind::Disconnect {
                    user: self.user,
                    tenant: self.tenant,
                },
            )) {
                let _ = self.ws.send_with_u8_array(&bytes);
            }
            let _ = self.ws.close();
        }
    }

    impl JsDb {
        async fn send_raw(&self, kind: RequestKind) -> Result<TransportResponse, JsValue> {
            let seq = self.seq.get();
            self.seq.set(seq + 1);

            let (tx, rx) = oneshot::channel::<TransportResponse>();
            self.pending.borrow_mut().insert(seq, tx);

            let req = TransportRequest::new(seq, kind);
            let bytes = encode_payload(&req).map_err(|e| JsValue::from_str(&e.to_string()))?;

            self.ws.send_with_u8_array(&bytes)?;

            rx.await
                .map_err(|_| JsValue::from_str("WebSocket disconnected"))
        }
    }

    // ── wasm-bindgen-test tests ───────────────────────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::*;
        use wasm_bindgen_test::*;

        wasm_bindgen_test_configure!(run_in_browser);

        #[wasm_bindgen_test]
        fn request_serialisation_roundtrip() {
            let req = TransportRequest::new(
                7,
                RequestKind::Connect {
                    user: 1,
                    tenant: 42,
                },
            );
            let bytes = encode_payload(&req).unwrap();
            let decoded: TransportRequest = decode_payload(&bytes).unwrap();
            assert_eq!(decoded.seq, 7);
            assert!(matches!(
                decoded.kind,
                RequestKind::Connect {
                    user: 1,
                    tenant: 42
                }
            ));
        }

        #[wasm_bindgen_test]
        fn response_serialisation_roundtrip() {
            let resp = TransportResponse {
                seq: 3,
                payload: b"hello_wasm".to_vec(),
                owner_url: Some("ws://127.0.0.1:7700".into()),
                backup_url: None,
                notifications: Vec::new(),
            };
            let bytes = encode_payload(&resp).unwrap();
            let decoded: TransportResponse = decode_payload(&bytes).unwrap();
            assert_eq!(decoded.seq, 3);
            assert_eq!(decoded.payload, b"hello_wasm");
            assert_eq!(decoded.owner_url.as_deref(), Some("ws://127.0.0.1:7700"));
        }

        #[wasm_bindgen_test]
        fn pending_map_dispatch() {
            let pending: PendingMap = Rc::new(RefCell::new(HashMap::new()));
            let (tx, _rx) = oneshot::channel::<TransportResponse>();
            pending.borrow_mut().insert(42, tx);
            assert_eq!(pending.borrow().len(), 1);
            // remove returns the sender
            assert!(pending.borrow_mut().remove(&42).is_some());
            assert!(pending.borrow_mut().remove(&99).is_none());
        }

        #[wasm_bindgen_test]
        fn write_request_encodes_payload() {
            let kind = RequestKind::Write {
                struct_id: 1,
                user: 7,
                tenant: 42,
                payload: b"data".to_vec(),
            };
            let req = TransportRequest::new(1, kind);
            let bytes = encode_payload(&req).unwrap();
            let decoded: TransportRequest = decode_payload(&bytes).unwrap();
            if let RequestKind::Write { payload, .. } = decoded.kind {
                assert_eq!(payload, b"data");
            } else {
                panic!("wrong kind");
            }
        }
    }
}

// Re-export public types so `wasm-pack build` exposes them at the crate root.
#[cfg(target_arch = "wasm32")]
pub use wasm_impl::JsDb;
