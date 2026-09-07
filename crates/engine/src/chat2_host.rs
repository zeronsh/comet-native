//! chat2 host wiring (docs/chat2-sync.md C3): the engine-side implementations
//! of [`zeron_sync::chat_client::ChatDocSink`] and
//! [`zeron_sync::chat_client::CheckpointFetcher`], binding a
//! [`crate::doc_host::ChatDocHandle`]'s live doc to a chat2 room.
//!
//! The C2 rule is enforced HERE: every sink method persists doc content AND
//! the room cursor in one `save_snapshot_with_cursor` transaction, so a
//! restored backup can never disagree with its own cursor — the root cause
//! of the redownload-forever class the old s2 clients suffered.

use std::sync::Arc;

use futures::future::BoxFuture;
use zeron_doc::SessionDoc;
use zeron_sync::chat_client::{ChatDocSink, CheckpointFetcher, RowImportOutcome};
use zeron_sync::{DocsStore, SyncError};

use crate::doc_host::EdgeConfig;

/// Doc epoch stamped on every chat2-synced snapshot (docs/chat2-sync.md M1:
/// thin docs are lineage epoch 2; M3 readers discard-and-adopt below it).
pub const CHAT2_DOC_EPOCH: u32 = 2;

/// [`ChatDocSink`] over a live [`SessionDoc`] + the cursor-bearing store.
///
/// Loro import of a remote row/checkpoint fires the doc's root subscription,
/// so the transcript watch, command drain, and debounced UI publish all ride
/// the existing change plumbing — this type only owns import + same-tx
/// persistence.
pub struct EngineChatSink {
    /// WEAK: the sink lives inside the handle's `ChatClient` for the
    /// client's whole life — a strong ref here kept
    /// `Arc::strong_count(&handle.doc) > 1` permanently, which reads as
    /// "live writer" to `pinned()` and made every chat2 handle immune to
    /// LRU eviction (unbounded warm-doc growth). Callbacks upgrade per
    /// call; a dead doc (evicted handle) is a no-op.
    doc: std::sync::Weak<SessionDoc>,
    store: Arc<DocsStore>,
    chat_id: String,
}

impl EngineChatSink {
    pub fn new(doc: &Arc<SessionDoc>, store: Arc<DocsStore>, chat_id: impl Into<String>) -> Self {
        Self {
            doc: Arc::downgrade(doc),
            store,
            chat_id: chat_id.into(),
        }
    }

    /// Export the CURRENT doc and persist it with `cursor` in one tx.
    fn persist_with_cursor(&self, cursor: u64) {
        let Some(doc) = self.doc.upgrade() else {
            return;
        };
        match doc.export_snapshot() {
            Ok(bytes) => {
                if let Err(err) = self.store.save_snapshot_with_cursor(
                    &self.chat_id,
                    &bytes,
                    cursor,
                    CHAT2_DOC_EPOCH,
                ) {
                    tracing::warn!(chat = %self.chat_id, error = %err,
                        "chat2 sink: snapshot persist failed (will retry on next change)");
                }
            }
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err,
                    "chat2 sink: snapshot export failed");
            }
        }
    }
}

impl ChatDocSink for EngineChatSink {
    fn apply_row(&self, bytes: &[u8], cursor: u64) -> RowImportOutcome {
        let Some(doc) = self.doc.upgrade() else {
            return RowImportOutcome::Applied;
        };
        match doc.doc().import(bytes) {
            Ok(status) => {
                if status.pending.is_some() {
                    // Room sequence contiguity does not prove causal history
                    // is present. Snapshot export omits parked operations;
                    // advancing its cursor would lose them after restart.
                    tracing::warn!(chat = %self.chat_id, cursor,
                        "chat2 sink: row parked on missing deps; requesting repair");
                    return RowImportOutcome::PendingDependencies;
                }
            }
            Err(err) => {
                // Malformed remote bytes cost the row, never the doc (the same
                // skip-not-fail rule as transcript reads). The cursor still
                // advances: replaying a poison row forever is the wedge class.
                tracing::warn!(chat = %self.chat_id, error = %err,
                    "chat2 sink: row import failed; skipping row");
            }
        }
        self.persist_with_cursor(cursor);
        RowImportOutcome::Applied
    }

    fn apply_checkpoint(&self, bytes: &[u8], cursor: u64) -> Result<(), String> {
        let doc = self.doc.upgrade().ok_or("doc evicted")?;
        let status = doc
            .doc()
            .import(bytes)
            .map_err(|e| format!("checkpoint import: {e}"))?;
        if status.pending.is_some() {
            return Err("checkpoint is missing causal dependencies".into());
        }
        self.persist_with_cursor(cursor);
        Ok(())
    }

    fn contains_frontier(&self, frontier: &[u8]) -> bool {
        let Some(doc) = self.doc.upgrade() else {
            return true; // evicted: claim contained so the client idles, not refetches
        };
        // NOTE deliberately no empty-frontier shortcut: an empty payload on
        // a PRESENT checkpoint is unreadable provenance, not proof there is
        // nothing to fetch — that shortcut made every fresh reader of such a
        // room skip the chat's founding ops and park all dependent rows
        // invisibly ("Add Tweets" incident, 2026-08-18). Empty falls through
        // to the decode failure below: NOT contained, fetch the checkpoint —
        // always safe (full-state merge; an empty-doc seed applies as a
        // no-op), never silently skips history. Callers already short-circuit
        // the checkpointSize == 0 (no checkpoint at all) case.
        let Ok(vv) = loro::VersionVector::decode(frontier) else {
            // Unreadable frontier → claim NOT contained: the client then
            // fetches the checkpoint, which is always safe (full-state
            // merge), never silently skips history.
            tracing::info!(chat = %self.chat_id, bytes = frontier.len(),
                "chat2 frontier unreadable; fetching checkpoint");
            return false;
        };
        // A decoded-but-EMPTY version vector is the vacuous claim: every doc
        // "includes" empty, so the check would pass for readers that hold
        // NOTHING and they'd skip the chat's founding ops (the actual "Add
        // Tweets" poison, one representation deeper than zero-length bytes).
        // A checkpoint the callers care about (size > 0) claiming empty
        // state is a contradiction — fetch it.
        if vv.is_empty() {
            tracing::info!(chat = %self.chat_id,
                "chat2 frontier decodes empty (vacuous); fetching checkpoint");
            return false;
        }
        doc.doc().oplog_vv().includes_vv(&vv)
    }

    fn advance_cursor(&self, cursor: u64) {
        self.persist_with_cursor(cursor);
    }
}

/// `GET /chat2/{chatId}/checkpoint` with Range resume — the fetcher half of
/// the C1 client contract. Partial downloads resume at the byte offset where
/// the previous attempt died (the DO serves 206), which is the entire point
/// of checkpoint-over-HTTP on the 1.2 Mbps links this design targets.
pub struct EdgeCheckpointFetcher {
    http: reqwest::Client,
    edge: EdgeConfig,
    chat_id: String,
}

impl EdgeCheckpointFetcher {
    pub fn new(http: reqwest::Client, edge: EdgeConfig, chat_id: impl Into<String>) -> Self {
        Self {
            http,
            edge,
            chat_id: chat_id.into(),
        }
    }
}

impl CheckpointFetcher for EdgeCheckpointFetcher {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        let http = self.http.clone();
        let edge = self.edge.clone();
        let url = format!(
            "{}/chat2/{}/checkpoint",
            edge.url.trim_end_matches('/'),
            self.chat_id
        );
        Box::pin(async move {
            let mut got: Vec<u8> = Vec::new();
            let mut seen_seq: Option<String> = None;
            // Range-resume loop: each attempt continues at the byte where
            // the last one stopped. Attempt count bounds a flapping link;
            // the ChatClient's own deadline bounds wall clock.
            for _attempt in 0..4 {
                let bearer = edge
                    .bearer()
                    .await
                    .ok_or_else(|| SyncError::Auth("signed out".into()))?;
                let mut req = http.get(&url).bearer_auth(&bearer);
                if !got.is_empty() {
                    req = req.header("range", format!("bytes={}-", got.len()));
                }
                let res = match req.send().await {
                    Ok(res) => res,
                    Err(err) => {
                        tracing::warn!(error = %err, "chat2 checkpoint fetch attempt failed");
                        continue;
                    }
                };
                // Resume validator: a NEW checkpoint can commit between
                // attempts, and a Range against it would splice two different
                // blobs (the import fails and burns a whole redial cycle).
                // The DO stamps every response with the checkpoint's seq —
                // on change, restart the download from byte 0.
                let seq = res
                    .headers()
                    .get("x-chat2-checkpoint-seq")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                if seq.is_some() && seen_seq.is_some() && seq != seen_seq {
                    tracing::info!(
                        resumed_at = got.len(),
                        "chat2 checkpoint replaced mid-download; restarting from 0"
                    );
                    got.clear();
                    seen_seq = seq;
                    continue;
                }
                if seq.is_some() {
                    seen_seq = seq;
                }
                match res.status().as_u16() {
                    200 => got.clear(),
                    206 => {}
                    416 => return Err(SyncError::Protocol("checkpoint range beyond end".into())),
                    404 => return Err(SyncError::Protocol("no checkpoint".into())),
                    code => return Err(SyncError::Protocol(format!("checkpoint HTTP {code}"))),
                }
                let mut stream = res;
                loop {
                    match stream.chunk().await {
                        Ok(Some(chunk)) => got.extend_from_slice(&chunk),
                        Ok(None) => return Ok(got),
                        Err(err) => {
                            // Mid-body drop: keep the bytes, resume via Range.
                            tracing::warn!(error = %err, resumed_at = got.len(),
                                "chat2 checkpoint stream dropped; resuming");
                            break;
                        }
                    }
                }
            }
            Err(SyncError::Protocol(
                "checkpoint fetch exhausted resume attempts".into(),
            ))
        })
    }
}

/// Plain-HTTPS chat pull/push (the airplane-wifi transport): GET/POST
/// `/chat2/{id}/rows` with the same bearer auth the checkpoint fetcher uses.
pub struct EdgeChatTransport {
    http: reqwest::Client,
    edge: EdgeConfig,
    chat_id: String,
    device_id: String,
}

impl EdgeChatTransport {
    pub fn new(
        http: reqwest::Client,
        edge: EdgeConfig,
        chat_id: impl Into<String>,
        device_id: impl Into<String>,
    ) -> Self {
        Self {
            http,
            edge,
            chat_id: chat_id.into(),
            device_id: device_id.into(),
        }
    }

    fn rows_url(&self) -> String {
        format!(
            "{}/chat2/{}/rows",
            self.edge.url.trim_end_matches('/'),
            self.chat_id
        )
    }
}

impl zeron_sync::chat_client::ChatTransport for EdgeChatTransport {
    fn fetch_rows(&self, after: u64) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        let http = self.http.clone();
        let edge = self.edge.clone();
        let url = self.rows_url();
        let device = self.device_id.clone();
        Box::pin(async move {
            let bearer = edge
                .bearer()
                .await
                .ok_or_else(|| SyncError::Auth("signed out".into()))?;
            let res = http
                .get(&url)
                .query(&[("after", after.to_string()), ("device", device)])
                .bearer_auth(&bearer)
                .send()
                .await
                .map_err(|e| SyncError::WebSocket(e.to_string()))?;
            if !res.status().is_success() {
                return Err(SyncError::Protocol(format!(
                    "chat pull http {}",
                    res.status()
                )));
            }
            let bytes = res
                .bytes()
                .await
                .map_err(|e| SyncError::WebSocket(e.to_string()))?;
            Ok(bytes.to_vec())
        })
    }

    fn push(
        &self,
        batch_id: String,
        bytes: Vec<u8>,
    ) -> BoxFuture<'static, Result<String, SyncError>> {
        let http = self.http.clone();
        let edge = self.edge.clone();
        let url = self.rows_url();
        let device = self.device_id.clone();
        Box::pin(async move {
            let bearer = edge
                .bearer()
                .await
                .ok_or_else(|| SyncError::Auth("signed out".into()))?;
            let res = http
                .post(&url)
                .query(&[("batchId", batch_id), ("device", device)])
                .bearer_auth(&bearer)
                .body(bytes)
                .send()
                .await
                .map_err(|e| SyncError::WebSocket(e.to_string()))?;
            if !res.status().is_success() {
                return Err(SyncError::Protocol(format!(
                    "chat push http {}",
                    res.status()
                )));
            }
            res.text()
                .await
                .map_err(|e| SyncError::WebSocket(e.to_string()))
        })
    }
}

#[cfg(test)]
mod frontier_tests {
    use super::*;
    use std::sync::Arc;

    /// The empty-frontier-means-contained shortcut skipped the chat's
    /// founding ops for every fresh reader of a room whose checkpoint
    /// carries an empty frontier label, parking all dependent rows
    /// invisibly ("Add Tweets" incident, 2026-08-18). An empty frontier on
    /// a present checkpoint must read as NOT contained — the fetch is
    /// always safe; the skip never is.
    /// The 2026-08-18 room's actual poison, one level deeper: a frontier
    /// that is a VALID ENCODING of an EMPTY version vector. Any doc
    /// vacuously "includes" empty, so the containment check said yes and
    /// fresh readers skipped the checkpoint anyway. A vacuous claim is not
    /// containment.
    #[test]
    fn encoded_empty_frontier_is_not_contained() {
        let dir = std::env::temp_dir().join(format!("zeron-frontier2-{}", std::process::id()));
        let store = Arc::new(DocsStore::open(&dir).expect("store opens"));
        let doc = Arc::new(SessionDoc::from_doc(loro::LoroDoc::new()));
        let sink = EngineChatSink::new(&doc, store, "frontier-test-2");
        let encoded_empty = loro::VersionVector::default().encode();
        assert!(
            !sink.contains_frontier(&encoded_empty),
            "an encoded-empty frontier must trigger the fetch, not vacuous containment"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_frontier_is_not_contained() {
        let dir = std::env::temp_dir().join(format!("zeron-frontier-test-{}", std::process::id()));
        let store = Arc::new(DocsStore::open(&dir).expect("store opens"));
        let doc = Arc::new(SessionDoc::from_doc(loro::LoroDoc::new()));
        let sink = EngineChatSink::new(&doc, store, "frontier-test");
        assert!(
            !sink.contains_frontier(&[]),
            "empty frontier on a present checkpoint must trigger the fetch"
        );
        // A real, contained frontier still short-circuits the fetch — the
        // doc needs actual ops, or its own frontier is the vacuous-empty one.
        doc.doc().get_map("meta").insert("k", "v").expect("insert");
        doc.doc().commit();
        let vv = doc.doc().oplog_vv().encode();
        assert!(sink.contains_frontier(&vv));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parked_rows_do_not_persist_a_cursor_until_their_history_arrives() {
        let source = SessionDoc::init("chat").unwrap();
        let text = source.doc().get_text("body");
        text.insert(0, "parent").unwrap();
        source.doc().commit();
        let checkpoint = source.export_snapshot().unwrap();
        let frontier = source.doc().oplog_vv();
        text.insert(6, " child").unwrap();
        source.doc().commit();
        let row = source
            .doc()
            .export(loro::ExportMode::updates(&frontier))
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let target = Arc::new(SessionDoc::from_doc(loro::LoroDoc::new()));
        let sink = EngineChatSink::new(&target, store.clone(), "chat");
        sink.persist_with_cursor(0);
        assert_eq!(
            sink.apply_row(&row, 1),
            RowImportOutcome::PendingDependencies
        );
        let (_, cursor, _) = store.load_snapshot_with_cursor("chat").unwrap().unwrap();
        assert_eq!(cursor, 0, "a restart must retry the invisible update");

        sink.apply_checkpoint(&checkpoint, 0).unwrap();
        assert_eq!(sink.apply_row(&row, 1), RowImportOutcome::Applied);
        let (snapshot, cursor, _) = store.load_snapshot_with_cursor("chat").unwrap().unwrap();
        assert_eq!(cursor, 1);
        let restored = loro::LoroDoc::new();
        restored.import(&snapshot).unwrap();
        assert_eq!(restored.get_text("body").to_string(), "parent child");

        let incomplete = Arc::new(SessionDoc::from_doc(loro::LoroDoc::new()));
        let incomplete_sink = EngineChatSink::new(&incomplete, store.clone(), "incomplete");
        assert!(incomplete_sink.apply_checkpoint(&row, 1).is_err());
        assert!(
            store
                .load_snapshot_with_cursor("incomplete")
                .unwrap()
                .is_none()
        );
    }
}
