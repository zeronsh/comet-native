//! Live cross-language integration check: drive the REAL `ChatClient` (real
//! tungstenite transport, real backoff/liveness actor) against a deployed
//! chat2 room, with a real Loro doc behind the sink. A JS peer (see
//! edge/scripts/chat2-crosscheck.mjs) seeds the room so both catch-up legs run:
//! checkpoint-then-rows on a fresh doc, then live push/ack.
//!
//! Usage:
//!   cargo run -p zeron-sync --example chat2_live -- <baseUrl> <chatId> <token> <device>
//!
//! Prints a single JSON result line prefixed RESULT: for the driver to parse.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use loro::{ExportMode, LoroDoc, VersionVector};
use zeron_sync::SyncError;
use zeron_sync::chat_client::{ChatClient, ChatDocSink, CheckpointFetcher, RowImportOutcome};

struct DocSink {
    doc: Mutex<LoroDoc>,
    cursor: AtomicU64,
    checkpoint_applied: AtomicBool,
    rows_applied: AtomicU64,
}

impl ChatDocSink for DocSink {
    fn apply_row(&self, bytes: &[u8], cursor: u64) -> RowImportOutcome {
        let doc = self.doc.lock().unwrap();
        if doc.import(bytes).expect("row import").pending.is_some() {
            return RowImportOutcome::PendingDependencies;
        }
        self.cursor.store(cursor, Relaxed);
        self.rows_applied.fetch_add(1, Relaxed);
        RowImportOutcome::Applied
    }
    fn apply_checkpoint(&self, bytes: &[u8], cursor: u64) -> Result<(), String> {
        let doc = self.doc.lock().unwrap();
        doc.import(bytes).map_err(|e| e.to_string())?;
        self.cursor.store(cursor, Relaxed);
        self.checkpoint_applied.store(true, Relaxed);
        Ok(())
    }
    fn contains_frontier(&self, frontier: &[u8]) -> bool {
        if frontier.is_empty() {
            return true;
        }
        let Ok(vv) = VersionVector::decode(frontier) else {
            return false;
        };
        self.doc.lock().unwrap().oplog_vv().includes_vv(&vv)
    }
    fn advance_cursor(&self, cursor: u64) {
        self.cursor.store(cursor, Relaxed);
    }
}

struct HttpFetcher {
    url: String,
    token: String,
}

impl CheckpointFetcher for HttpFetcher {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        let url = self.url.clone();
        let token = self.token.clone();
        Box::pin(async move {
            let res = reqwest::Client::new()
                .get(&url)
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|e| SyncError::WebSocket(e.to_string()))?;
            if !res.status().is_success() {
                return Err(SyncError::WebSocket(format!("checkpoint {}", res.status())));
            }
            let bytes = res
                .bytes()
                .await
                .map_err(|e| SyncError::WebSocket(e.to_string()))?;
            Ok(bytes.to_vec())
        })
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (base, chat, token, device) = (&args[1], &args[2], &args[3], &args[4]);
    let ws_url = format!(
        "{}/chat2/{}/ws?device={}&token={}",
        base.replace("https://", "wss://"),
        chat,
        device,
        token
    );

    let sink = Arc::new(DocSink {
        doc: Mutex::new(LoroDoc::new()),
        cursor: AtomicU64::new(0),
        checkpoint_applied: AtomicBool::new(false),
        rows_applied: AtomicU64::new(0),
    });
    let fetcher = Arc::new(HttpFetcher {
        url: format!("{base}/chat2/{chat}/checkpoint"),
        token: token.clone(),
    });

    // Fresh doc, cursor 0 → the checkpoint frontier can't be contained →
    // the client must take the CheckpointThenRows leg (GET + import + rows).
    let client = ChatClient::connect(&ws_url, sink.clone(), fetcher, device, 0)
        .await
        .expect("connect");

    let caught_up_cursor = sink.cursor.load(Relaxed);
    let caught_up_text = sink.doc.lock().unwrap().get_text("t").to_string();

    // Live push: append through the real doc, export the delta, enqueue.
    let update = {
        let doc = sink.doc.lock().unwrap();
        let before = doc.oplog_vv();
        doc.get_text("t")
            .insert(caught_up_text.len(), " rust-was-here")
            .expect("insert");
        doc.commit();
        doc.export(ExportMode::updates(&before)).expect("export")
    };
    client.enqueue_update(update);

    // Wait for the ack to advance the cursor past the caught-up head.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while sink.cursor.load(Relaxed) <= caught_up_cursor {
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let result = serde_json::json!({
        "checkpointApplied": sink.checkpoint_applied.load(Relaxed),
        "rowsApplied": sink.rows_applied.load(Relaxed),
        "caughtUpCursor": caught_up_cursor,
        "finalCursor": sink.cursor.load(Relaxed),
        "text": sink.doc.lock().unwrap().get_text("t").to_string(),
        "messages": sink.doc.lock().unwrap().get_list("messages").len(),
    });
    println!("RESULT:{result}");
    client.shutdown().await;
}
