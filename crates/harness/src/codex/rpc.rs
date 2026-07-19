//! Minimal JSON-RPC 2.0 client over the app server's stdio (newline-delimited
//! frames, id-multiplexed), ported from codex.ts's `startAppServer`.
//!
//! - Responses are matched to callers by numeric id (a shared pending map the
//!   reader task resolves directly, so requests can be awaited from anywhere —
//!   including inside the session loop — without starving notifications).
//! - Notifications and server→client requests (approvals) are pumped into an
//!   [`Incoming`] channel the session loop drains.
//! - Writes to a dead child's stdin (EPIPE) are tolerated and logged, matching
//!   the TS harness's swallowed-EPIPE behavior.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

use crate::HarnessError;

/// A non-response line from the app server, in stdout order.
#[derive(Debug)]
pub(crate) enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// Server→client request (approvals); must be answered via
    /// [`RpcClient::respond`] / [`RpcClient::respond_error`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// stdout EOF: the app server exited. All pending requests fail.
    Eof,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Clone)]
pub(crate) struct RpcClient {
    next_id: Arc<AtomicI64>,
    pending: Pending,
    writer: mpsc::UnboundedSender<String>,
}

impl RpcClient {
    /// Spawn the writer + reader tasks over the child's stdio; returns the
    /// client and the incoming (notification/request) channel.
    pub fn new(stdin: ChildStdin, stdout: ChildStdout) -> (Self, mpsc::Receiver<Incoming>) {
        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(write_loop(stdin, writer_rx));
        let pending: Pending = Arc::default();
        let (incoming_tx, incoming_rx) = mpsc::channel(256);
        tokio::spawn(read_loop(stdout, Arc::clone(&pending), incoming_tx));
        (
            Self {
                next_id: Arc::new(AtomicI64::new(0)),
                pending,
                writer: writer_tx,
            },
            incoming_rx,
        )
    }

    /// Send a request and await its response (resolved by the reader task).
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, HarnessError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.writer.send(line.to_string()).is_err() {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(HarnessError::Protocol(format!(
                "{method}: app-server stdin closed"
            )));
        }
        match rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(message)) => Err(HarnessError::Protocol(format!("{method}: {message}"))),
            // Sender dropped: the reader hit EOF and failed all pending.
            Err(_) => Err(HarnessError::Protocol(format!(
                "{method}: app-server exited before responding"
            ))),
        }
    }

    /// Fire a notification (no id, no response).
    pub fn notify(&self, method: &str, params: Option<Value>) {
        let line = match params {
            Some(params) => json!({ "jsonrpc": "2.0", "method": method, "params": params }),
            None => json!({ "jsonrpc": "2.0", "method": method }),
        };
        let _ = self.writer.send(line.to_string());
    }

    /// Answer a server→client request.
    pub fn respond(&self, id: &Value, result: Value) {
        let line = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = self.writer.send(line.to_string());
    }

    /// Reject a server→client request (e.g. unknown method).
    pub fn respond_error(&self, id: &Value, code: i64, message: &str) {
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        });
        let _ = self.writer.send(line.to_string());
    }
}

/// Owns the child's stdin; a write failure (EPIPE after the child died) is
/// tolerated and logged.
async fn write_loop(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = rx.recv().await {
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if let Err(e) = write.await {
            tracing::debug!(target: "comet_harness::codex", "stdin write failed (tolerated): {e}");
            return;
        }
    }
}

/// Parse stdout lines: responses resolve the pending map, everything else is
/// forwarded in order. Non-JSON noise is skipped; on EOF all pending requests
/// fail (their senders drop) and one final [`Incoming::Eof`] is delivered.
async fn read_loop(stdout: ChildStdout, pending: Pending, tx: mpsc::Sender<Incoming>) {
    let mut lines = BufReader::new(stdout).lines();
    // A read error ends the loop like EOF: either way the child's stdout is
    // unusable, pending requests must fail, and the session loop must know.
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            tracing::debug!(target: "comet_harness::codex", "non-JSON stdout line (skipped)");
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id");
        match (method, id) {
            // Response: resolve the awaiting request.
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let Some(sender) = pending.lock().expect("pending lock").remove(&id) else {
                    continue;
                };
                let outcome = match msg.get("error") {
                    Some(err) => Err(err
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| err.to_string())),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = sender.send(outcome);
            }
            // Server→client request (approvals).
            (Some(method), Some(id)) => {
                let incoming = Incoming::Request {
                    id: id.clone(),
                    method: method.to_owned(),
                    params: msg.get("params").cloned().unwrap_or(Value::Null),
                };
                if tx.send(incoming).await.is_err() {
                    return;
                }
            }
            // Notification.
            (Some(method), None) => {
                let incoming = Incoming::Notification {
                    method: method.to_owned(),
                    params: msg.get("params").cloned().unwrap_or(Value::Null),
                };
                if tx.send(incoming).await.is_err() {
                    return;
                }
            }
            (None, None) => {}
        }
    }
    // EOF/read error: fail every awaiting request, then signal the loop.
    pending.lock().expect("pending lock").clear();
    let _ = tx.send(Incoming::Eof).await;
}
