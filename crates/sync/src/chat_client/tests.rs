//! ChatClient behavior against a hand-driven server end (channel pipes — no
//! WebSocket): handshake precision, backfill, push/ack retirement, and the
//! reconnect re-push path. Virtual clock (`start_paused`) so backoff and
//! deadlines cost nothing.

use std::collections::VecDeque;
use std::sync::Mutex;

use super::*;
use crate::chat_frames::{decode, encode, frame_type};

// ── plumbing: linked pipes + scripted connector ─────────────────────────────

struct ServerEnd {
    tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
}

fn pipe_pair() -> (BinPipe, ServerEnd) {
    let (c2s_tx, c2s_rx) = mpsc::channel(64);
    let (s2c_tx, s2c_rx) = mpsc::channel(64);
    (
        BinPipe {
            tx: c2s_tx,
            rx: s2c_rx,
        },
        ServerEnd {
            tx: s2c_tx,
            rx: c2s_rx,
        },
    )
}

struct ChanConnector {
    pipes: Mutex<VecDeque<BinPipe>>,
}

impl BinConnector for ChanConnector {
    fn connect(&self) -> BoxFuture<'static, Result<BinPipe, SyncError>> {
        let pipe = lock(&self.pipes).pop_front();
        Box::pin(async move { pipe.ok_or(SyncError::Closed) })
    }
}

// ── sink + fetcher doubles ──────────────────────────────────────────────────

#[derive(Default)]
struct RecordingSink {
    rows: Mutex<Vec<(Vec<u8>, u64)>>,
    checkpoints: Mutex<Vec<(Vec<u8>, u64)>>,
    cursor_advances: Mutex<Vec<u64>>,
    frontier_contained: std::sync::atomic::AtomicBool,
    pending_until_checkpoint: std::sync::atomic::AtomicBool,
    /// Global apply order across rows and checkpoints — the overlap test
    /// pins "checkpoint imports before any row that buffered during it".
    ops: Mutex<Vec<String>>,
}

impl ChatDocSink for RecordingSink {
    fn apply_row(&self, bytes: &[u8], cursor: u64) -> RowImportOutcome {
        if self
            .pending_until_checkpoint
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return RowImportOutcome::PendingDependencies;
        }
        lock(&self.rows).push((bytes.to_vec(), cursor));
        lock(&self.ops).push(format!("row@{cursor}"));
        RowImportOutcome::Applied
    }
    fn apply_checkpoint(&self, bytes: &[u8], cursor: u64) -> Result<(), String> {
        self.pending_until_checkpoint
            .store(false, std::sync::atomic::Ordering::Relaxed);
        lock(&self.checkpoints).push((bytes.to_vec(), cursor));
        lock(&self.ops).push(format!("ckpt@{cursor}"));
        Ok(())
    }
    fn contains_frontier(&self, _frontier: &[u8]) -> bool {
        self.frontier_contained
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    fn advance_cursor(&self, cursor: u64) {
        lock(&self.cursor_advances).push(cursor);
    }
}

struct FixedFetcher {
    bytes: Vec<u8>,
    calls: Arc<std::sync::atomic::AtomicU64>,
}

impl CheckpointFetcher for FixedFetcher {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let bytes = self.bytes.clone();
        Box::pin(async move { Ok(bytes) })
    }
}

// ── server-side script helpers ──────────────────────────────────────────────

async fn expect_kind(end: &mut ServerEnd, kind: u8) -> wire::WireFrame {
    loop {
        let bytes = end.rx.recv().await.expect("client hung up");
        let frame = decode(&bytes).expect("client sent undecodable frame");
        if frame.kind == kind {
            return frame;
        }
        panic!("expected frame {kind:#x}, got {:#x}", frame.kind);
    }
}

async fn send(end: &ServerEnd, kind: u8, header: serde_json::Value, payload: &[u8]) {
    end.tx.send(encode(kind, &header, payload)).await.unwrap();
}

/// Answer hello with `state`, then serve the rows request with `rows`.
/// Returns the observed `after` from the rows request. `expect_exclude`
/// pins the F1 rule: the process's FIRST backfill must redownload own rows
/// (false), same-process reconnects skip them (true).
async fn serve_join(
    end: &mut ServerEnd,
    state: serde_json::Value,
    frontier: &[u8],
    rows: Vec<(u64, &str, Vec<u8>)>,
    expect_exclude: bool,
) -> u64 {
    let hello = expect_kind(end, frame_type::HELLO).await;
    assert!(hello.header["device"].is_string());
    let head_seq = state["headSeq"].as_u64().unwrap();
    send(end, frame_type::STATE, state, frontier).await;
    let req = expect_kind(end, frame_type::ROWS_REQ).await;
    assert_eq!(req.header["excludeOwn"], expect_exclude);
    let after = req.header["after"].as_u64().unwrap();
    for (seq, device, bytes) in rows {
        send(
            end,
            frame_type::ROW,
            serde_json::json!({"seq": seq, "device": device, "batchId": format!("b{seq}")}),
            &bytes,
        )
        .await;
    }
    send(
        end,
        frame_type::ROWS_DONE,
        serde_json::json!({"headSeq": head_seq}),
        &[],
    )
    .await;
    after
}

fn connector(pipes: Vec<BinPipe>) -> Arc<ChanConnector> {
    Arc::new(ChanConnector {
        pipes: Mutex::new(pipes.into_iter().collect()),
    })
}

fn fetcher(bytes: &[u8]) -> (Arc<FixedFetcher>, Arc<std::sync::atomic::AtomicU64>) {
    let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
    (
        Arc::new(FixedFetcher {
            bytes: bytes.to_vec(),
            calls: calls.clone(),
        }),
        calls,
    )
}

// ── plan_catch_up (pure) ────────────────────────────────────────────────────

#[test]
fn catch_up_plan_covers_the_decision_table() {
    let state = |head: u64, ckpt: u64| wire::StateHeader {
        head_seq: head,
        seq_floor: ckpt,
        checkpoint_seq: ckpt,
        checkpoint_size: if ckpt > 0 { 1000 } else { 0 },
        row_count: 0,
        row_bytes: 0,
    };
    // Seeded-at-zero room (M1): checkpointSeq 0 but a real blob — the
    // presence test is SIZE; a fresh reader must fetch the seed.
    let seeded = wire::StateHeader {
        head_seq: 0,
        seq_floor: 0,
        checkpoint_seq: 0,
        checkpoint_size: 276_000,
        row_count: 0,
        row_bytes: 0,
    };
    assert_eq!(
        plan_catch_up(0, &seeded, false),
        CatchUpPlan::CheckpointThenRows { after: 0 }
    );
    assert_eq!(
        plan_catch_up(0, &seeded, true),
        CatchUpPlan::RowsOnly { after: 0 }
    );
    // Empty room / no checkpoint: rows from the cursor.
    assert_eq!(
        plan_catch_up(0, &state(0, 0), true),
        CatchUpPlan::RowsOnly { after: 0 }
    );
    assert_eq!(
        plan_catch_up(4, &state(9, 0), true),
        CatchUpPlan::RowsOnly { after: 4 }
    );
    // Frontier contained: skip the checkpoint even from an older cursor.
    assert_eq!(
        plan_catch_up(2, &state(9, 5), true),
        CatchUpPlan::RowsOnly { after: 5 }
    );
    assert_eq!(
        plan_catch_up(7, &state(9, 5), true),
        CatchUpPlan::RowsOnly { after: 7 }
    );
    // Frontier missing: checkpoint first, rows after it.
    assert_eq!(
        plan_catch_up(2, &state(9, 5), false),
        CatchUpPlan::CheckpointThenRows { after: 5 }
    );
    // Server lost state (cursor ahead of head): cursor is meaningless.
    assert_eq!(
        plan_catch_up(50, &state(3, 0), true),
        CatchUpPlan::RowsOnly { after: 0 }
    );
}

// ── end-to-end actor behavior ───────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn fresh_join_backfills_rows_and_advances_cursor() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, fetch_calls) = fetcher(b"");

    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 2, "seqFloor": 0, "checkpointSeq": 0,
                "checkpointSize": 0, "rowCount": 2, "rowBytes": 64}),
            &[],
            vec![(1, "dev-b", vec![0xaa]), (2, "dev-b", vec![0xbb])],
            false,
        )
        .await;
        assert_eq!(after, 0);
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    server.await.unwrap();

    assert_eq!(
        *lock(&sink.rows),
        vec![(vec![0xaa], 1), (vec![0xbb], 2)],
        "both remote rows imported in seq order"
    );
    assert_eq!(fetch_calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    let stats = client.stats();
    assert!(stats.connected);
    assert_eq!(stats.cursor, 2);
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn contained_frontier_skips_the_checkpoint_download() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    sink.frontier_contained
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let (fetch, fetch_calls) = fetcher(b"never");

    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 8, "seqFloor": 5, "checkpointSeq": 5,
                "checkpointSize": 160_000, "rowCount": 3, "rowBytes": 900}),
            &[1, 2, 3],
            vec![
                (6, "dev-b", vec![6]),
                (7, "dev-b", vec![7]),
                (8, "dev-b", vec![8]),
            ],
            false,
        )
        .await;
        // Client-side precision: cursor was 0 but the frontier is local —
        // skip straight past the checkpointed span.
        assert_eq!(after, 5);
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    server.await.unwrap();

    assert_eq!(fetch_calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(lock(&sink.checkpoints).is_empty());
    assert_eq!(lock(&sink.rows).len(), 3);
    assert_eq!(client.stats().cursor, 8);
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn missing_frontier_fetches_and_imports_the_checkpoint_first() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, fetch_calls) = fetcher(b"checkpoint-bytes");

    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 6, "seqFloor": 5, "checkpointSeq": 5,
                "checkpointSize": 16, "rowCount": 1, "rowBytes": 10}),
            &[9, 9, 9],
            vec![(6, "dev-b", vec![6])],
            false,
        )
        .await;
        assert_eq!(after, 5, "rows resume after the checkpoint");
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        2,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    server.await.unwrap();

    assert_eq!(fetch_calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        *lock(&sink.checkpoints),
        vec![(b"checkpoint-bytes".to_vec(), 5)]
    );
    assert_eq!(*lock(&sink.rows), vec![(vec![6u8], 6)]);
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn unacked_pushes_survive_reconnect_and_acks_retire_them() {
    let (pipe1, mut end1) = pipe_pair();
    let (pipe2, mut end2) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");

    let empty_state = serde_json::json!({"headSeq": 0, "seqFloor": 0,
        "checkpointSeq": 0, "checkpointSize": 0, "rowCount": 0, "rowBytes": 0});

    let s1 = tokio::spawn({
        let state = empty_state.clone();
        async move {
            serve_join(&mut end1, state, &[], vec![], false).await;
            // Receive the push but die before acking — the client must
            // re-push the SAME batch id on the next session.
            let push = expect_kind(&mut end1, frame_type::PUSH).await;
            let batch_id = push.header["batchId"].as_str().unwrap().to_string();
            assert_eq!(push.payload, vec![0xd1u8]);
            drop(end1); // socket dies
            batch_id
        }
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe1, pipe2]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");

    client.enqueue_update(vec![0xd1]);
    let first_batch = s1.await.unwrap();
    assert_eq!(
        client.stats().pending_pushes,
        1,
        "unacked batch stays queued"
    );

    // Second session: same handshake, then the replayed push gets acked.
    let s2 = tokio::spawn({
        let state = empty_state.clone();
        async move {
            serve_join(&mut end2, state, &[], vec![], true).await;
            let push = expect_kind(&mut end2, frame_type::PUSH).await;
            let batch_id = push.header["batchId"].as_str().unwrap().to_string();
            send(
                &end2,
                frame_type::ACK,
                serde_json::json!({"batchId": batch_id, "seq": 1, "dup": false}),
                &[],
            )
            .await;
            (batch_id, end2)
        }
    });
    let (replayed_batch, _keep_alive) = s2.await.unwrap();
    assert_eq!(
        replayed_batch, first_batch,
        "reconnect replays the same batch id"
    );

    // Ack lands asynchronously — wait for the pending queue to drain.
    let mut events = client.events();
    while client.stats().pending_pushes > 0 {
        let _ = events.recv().await;
    }
    assert_eq!(client.stats().cursor, 1, "ack advanced the cursor");
    assert_eq!(*lock(&sink.cursor_advances), vec![1]);
    client.shutdown().await;
}

// ── 2026-08-10 review fixes (F1–F4) ─────────────────────────────────────────

struct PendingFetcher;
impl CheckpointFetcher for PendingFetcher {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        Box::pin(std::future::pending())
    }
}

/// F2: a permanent server verdict (`too_large`) retires the batch from the
/// replay queue; a transient one (`quota`) keeps it and re-pushes on the
/// retry clock without waiting for a new enqueue.
#[tokio::test(start_paused = true)]
async fn permanent_rejection_retires_transient_keeps_and_retries() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");
    let empty_state = serde_json::json!({"headSeq": 0, "seqFloor": 0,
        "checkpointSeq": 0, "checkpointSize": 0, "rowCount": 0, "rowBytes": 0});

    let server = tokio::spawn(async move {
        serve_join(&mut end, empty_state, &[], vec![], false).await;
        // First batch: permanently rejected.
        let doomed = expect_kind(&mut end, frame_type::PUSH).await;
        let doomed_id = doomed.header["batchId"].as_str().unwrap().to_string();
        send(
            &end,
            frame_type::ERROR,
            serde_json::json!({"code": "too_large", "message": "push rejected", "batchId": doomed_id}),
            &[],
        )
        .await;
        // Second batch: quota-limited once, then replayed by the retry clock
        // (no further enqueue nudges) and acked.
        let quotad = expect_kind(&mut end, frame_type::PUSH).await;
        let quotad_id = quotad.header["batchId"].as_str().unwrap().to_string();
        send(
            &end,
            frame_type::ERROR,
            serde_json::json!({"code": "quota", "message": "later", "batchId": quotad_id}),
            &[],
        )
        .await;
        let replay = expect_kind(&mut end, frame_type::PUSH).await;
        assert_eq!(
            replay.header["batchId"].as_str().unwrap(),
            quotad_id,
            "retry clock replays the SAME quota-limited batch"
        );
        send(
            &end,
            frame_type::ACK,
            serde_json::json!({"batchId": quotad_id, "seq": 1, "dup": false}),
            &[],
        )
        .await;
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");

    let mut events = client.events();
    client.enqueue_update(vec![0xd0]); // doomed
    // Retirement lands asynchronously; PushRejected marks it.
    loop {
        if let Ok(ChatEvent::PushRejected) = events.recv().await {
            break;
        }
    }
    assert_eq!(client.stats().pending_pushes, 0, "doomed batch retired");

    client.enqueue_update(vec![0xb0]);
    let _keep = server.await.unwrap();
    while client.stats().pending_pushes > 0 {
        let _ = events.recv().await;
    }
    assert_eq!(client.stats().cursor, 1, "quota batch eventually landed");
    assert!(client.stats().rejected >= 2);
    client.shutdown().await;
}

/// F2: batches over the row cap never enter the replay queue.
#[tokio::test(start_paused = true)]
async fn oversized_enqueue_is_refused_at_the_door() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");
    let empty_state = serde_json::json!({"headSeq": 0, "seqFloor": 0,
        "checkpointSeq": 0, "checkpointSize": 0, "rowCount": 0, "rowBytes": 0});
    let server = tokio::spawn(async move {
        serve_join(&mut end, empty_state, &[], vec![], false).await;
        end
    });
    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink,
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    let _keep = server.await.unwrap();

    // Exactly the DO row cap (1 MiB): the frame header would push the WS
    // message over the runtime's 1 MiB cap and the socket would close with
    // NO error frame to retire the batch — the gate must refuse it (this is
    // why MAX_PUSH_BYTES carries headroom below the row cap).
    client.enqueue_update(vec![0u8; 1024 * 1024]);
    let stats = client.stats();
    assert_eq!(stats.pending_pushes, 0, "boundary batch not queued");
    assert_eq!(stats.rejected, 1);
    client.shutdown().await;
}

/// F4 (second half): `shutdown()` must complete promptly even while the
/// actor is parked inside a hung checkpoint fetch.
#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_a_hung_checkpoint_fetch() {
    let (pipe1, mut end1) = pipe_pair();
    let (pipe2, mut end2) = pipe_pair();
    let sink = Arc::new(RecordingSink::default()); // frontier NOT contained
    let empty_state = serde_json::json!({"headSeq": 0, "seqFloor": 0,
        "checkpointSeq": 0, "checkpointSize": 0, "rowCount": 0, "rowBytes": 0});

    // Session 1: clean join (no checkpoint), then the socket dies.
    let s1 = tokio::spawn(async move {
        serve_join(&mut end1, empty_state, &[], vec![], false).await;
        drop(end1);
    });
    // Session 2: a checkpoint appeared — the client must fetch, and hangs.
    let s2 = tokio::spawn(async move {
        let _hello = expect_kind(&mut end2, frame_type::HELLO).await;
        send(
            &end2,
            frame_type::STATE,
            serde_json::json!({"headSeq": 9, "seqFloor": 5, "checkpointSeq": 5,
                "checkpointSize": 1000, "rowCount": 4, "rowBytes": 40}),
            &[7, 7, 7],
        )
        .await;
        end2 // keep the pipe alive; only the fetch is stuck
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe1, pipe2]),
        sink,
        Arc::new(PendingFetcher),
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("first join succeeds");
    s1.await.unwrap();
    let _keep = s2.await.unwrap();
    // Let the actor redial and park inside the hung fetch.
    tokio::time::sleep(Duration::from_secs(2)).await;
    tokio::time::timeout(Duration::from_secs(30), client.shutdown())
        .await
        .expect("shutdown must not hang on a stuck fetch");
}

/// F3: a server whose headSeq fell behind our cursor (reset/wiped room) is
/// SURFACED — counted in stats, honest head_seq — not silently absorbed.
#[tokio::test(start_paused = true)]
async fn server_reset_is_counted_and_head_seq_stays_honest() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");
    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 3, "seqFloor": 0, "checkpointSeq": 0,
                "checkpointSize": 0, "rowCount": 3, "rowBytes": 30}),
            &[],
            vec![
                (1, "dev-b", vec![1]),
                (2, "dev-b", vec![2]),
                (3, "dev-b", vec![3]),
            ],
            false,
        )
        .await;
        assert_eq!(after, 0, "meaningless cursor treated as fresh");
        end
    });
    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink,
        fetch,
        "dev-a",
        50, // persisted cursor from before the room was wiped
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    let _keep = server.await.unwrap();

    let stats = client.stats();
    assert_eq!(stats.server_resets, 1, "reset visible to the host");
    assert_eq!(stats.head_seq, 3, "server view not masked by the cursor");
    assert_eq!(stats.cursor, 3, "cursor re-anchored by the backfill");
    client.shutdown().await;
}

/// F4: a checkpoint fetch that never resolves fails the first join within
/// the deadline instead of hanging the actor (and shutdown) forever.
#[tokio::test(start_paused = true)]
async fn hung_checkpoint_fetch_fails_the_join_within_deadline() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default()); // frontier NOT contained
    let server = tokio::spawn(async move {
        let hello = expect_kind(&mut end, frame_type::HELLO).await;
        assert!(hello.header["device"].is_string());
        send(
            &end,
            frame_type::STATE,
            serde_json::json!({"headSeq": 9, "seqFloor": 5, "checkpointSeq": 5,
                "checkpointSize": 1000, "rowCount": 4, "rowBytes": 40}),
            &[7, 7, 7],
        )
        .await;
        end // keep the pipe alive; the fetch is what must time out
    });
    let joined = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink,
        Arc::new(PendingFetcher),
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await;
    assert!(joined.is_err(), "hung fetch must not hang the join");
    let _keep = server.await.unwrap();
}

/// M1 seed shape: checkpointSeq 0 with a real blob. BOTH presence tests
/// (plan_catch_up AND run_session's frontier short-circuit) must key on
/// SIZE — the 2026-08-10 gauntlet caught seq==0 short-circuits in each,
/// which would have made every adopted reader skip the seed and render an
/// EMPTY transcript.
#[tokio::test(start_paused = true)]
async fn seeded_at_zero_room_fetches_the_checkpoint() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default()); // frontier NOT contained
    let (fetch, fetch_calls) = fetcher(b"seed-checkpoint-bytes");

    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 0, "seqFloor": 0, "checkpointSeq": 0,
                "checkpointSize": 276_342, "rowCount": 0, "rowBytes": 0}),
            &[7, 7, 7], // non-empty frontier the fresh doc can't contain
            vec![],
            false,
        )
        .await;
        assert_eq!(after, 0);
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    let _keep = server.await.unwrap();

    assert_eq!(
        fetch_calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "seed checkpoint fetched despite checkpointSeq == 0"
    );
    assert_eq!(
        *lock(&sink.checkpoints),
        vec![(b"seed-checkpoint-bytes".to_vec(), 0)]
    );
    client.shutdown().await;
}

// ── 450kbps cold-open: overlap + early push ─────────────────────────────────

/// Fetch that resolves only when the test releases its gate — stands in for
/// a checkpoint blob crawling down a thin link.
struct GatedFetcher {
    gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    bytes: Vec<u8>,
}

impl CheckpointFetcher for GatedFetcher {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        let gate = lock(&self.gate).take().expect("single fetch");
        let bytes = self.bytes.clone();
        Box::pin(async move {
            let _ = gate.await;
            Ok(bytes)
        })
    }
}

/// The rows request leaves BEFORE the checkpoint download finishes (the
/// server observes it while the fetch is still gated), rows landing
/// mid-download buffer, and the import still applies before any of them —
/// the join must not serialize download → request → backfill.
#[tokio::test]
async fn checkpoint_fetch_overlaps_row_backfill() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default()); // frontier NOT contained
    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel();
    let fetch = Arc::new(GatedFetcher {
        gate: Mutex::new(Some(gate_rx)),
        bytes: b"parallel-checkpoint".to_vec(),
    });

    let server = tokio::spawn(async move {
        let _hello = expect_kind(&mut end, frame_type::HELLO).await;
        send(
            &end,
            frame_type::STATE,
            serde_json::json!({"headSeq": 7, "seqFloor": 0,
                "checkpointSeq": 5, "checkpointSize": 1000,
                "rowCount": 2, "rowBytes": 64}),
            b"frontier",
        )
        .await;
        // The ordering pin: this arrives while the fetch is still GATED.
        let req = expect_kind(&mut end, frame_type::ROWS_REQ).await;
        assert_eq!(req.header["after"], 5, "rows resume past the checkpoint");
        send(
            &end,
            frame_type::ROW,
            serde_json::json!({"seq": 6, "device": "dev-b", "batchId": "b6"}),
            b"r6",
        )
        .await;
        send(
            &end,
            frame_type::ROW,
            serde_json::json!({"seq": 7, "device": "dev-b", "batchId": "b7"}),
            b"r7",
        )
        .await;
        send(
            &end,
            frame_type::ROWS_DONE,
            serde_json::json!({"headSeq": 7}),
            &[],
        )
        .await;
        // Only now does the "download" complete.
        let _ = gate_tx.send(());
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds with rows served before the checkpoint bytes");

    let _end = server.await.unwrap();
    assert_eq!(
        *lock(&sink.ops),
        vec!["ckpt@5", "row@6", "row@7"],
        "checkpoint imports before any row that buffered during the download"
    );
    assert_eq!(client.stats().cursor, 7);
    client.shutdown().await;
}

/// A batch queued while offline flushes right after the reconnect's state
/// answer — NOT after backfill converges. The server script only serves the
/// backfill AFTER seeing (and acking) the push; the old order deadlocks here.
#[tokio::test(start_paused = true)]
async fn pending_push_flushes_before_backfill_completes() {
    let (pipe_a, mut end_a) = pipe_pair();
    let (pipe_b, mut end_b) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    sink.frontier_contained
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let (fetch, _) = fetcher(b"");
    let empty_state = serde_json::json!({"headSeq": 0, "seqFloor": 0,
        "checkpointSeq": 0, "checkpointSize": 0, "rowCount": 0, "rowBytes": 0});

    let state_b = empty_state.clone();
    let server = tokio::spawn(async move {
        serve_join(&mut end_a, empty_state, &[], vec![], false).await;
        // The push arrives on session A but goes UNACKED; the socket dies.
        let _push = expect_kind(&mut end_a, frame_type::PUSH).await;
        drop(end_a);

        // Session B: the replay must arrive before ANY backfill is served.
        let _hello = expect_kind(&mut end_b, frame_type::HELLO).await;
        send(&end_b, frame_type::STATE, state_b, &[]).await;
        let _req = expect_kind(&mut end_b, frame_type::ROWS_REQ).await;
        let replay = expect_kind(&mut end_b, frame_type::PUSH).await;
        let batch_id = replay.header["batchId"].as_str().unwrap().to_string();
        send(
            &end_b,
            frame_type::ACK,
            serde_json::json!({"batchId": batch_id, "seq": 1, "dup": false}),
            &[],
        )
        .await;
        send(
            &end_b,
            frame_type::ROWS_DONE,
            serde_json::json!({"headSeq": 1}),
            &[],
        )
        .await;
        end_b
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe_a, pipe_b]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("first join succeeds");

    client.enqueue_update(vec![0xaa]);
    let mut events = client.events();
    // Ride events until the reconnect's early replay is acked.
    while client.stats().pending_pushes > 0 {
        let _ = events.recv().await;
    }
    let _end = server.await.unwrap();
    assert_eq!(
        client.stats().cursor,
        1,
        "early replay acked before ROWS_DONE"
    );
    client.shutdown().await;
}

/// A checkpoint that EXISTS (size > 0) but whose frontier payload is empty
/// must be fetched, not skipped: the empty-frontier-means-contained shortcut
/// made every fresh reader of such a room skip the chat's founding ops and
/// park all dependent rows invisibly ("Add Tweets" incident, 2026-08-18).
/// Fetching is always safe (full-state merge); skipping history never is.
#[test]
fn empty_frontier_with_real_checkpoint_is_not_contained() {
    let state = wire::StateHeader {
        head_seq: 75,
        seq_floor: 5,
        checkpoint_seq: 5,
        checkpoint_size: 2728,
        row_count: 70,
        row_bytes: 17150,
    };
    // The sink cannot vouch for a frontier it cannot read — an empty payload
    // must plan a checkpoint fetch for a cursor-0 reader.
    assert_eq!(
        plan_catch_up(0, &state, false),
        CatchUpPlan::CheckpointThenRows { after: 5 },
        "fresh reader must fetch a present checkpoint when the frontier is unreadable"
    );
}

// ── flaky-network suite (durable-by-design §Phase 3) ────────────────────────
//
// The deterministic drop harness: a scripted connector where each dial slot
// is either a live pipe or a refusal, with connect times recorded on the
// virtual clock. The contract under test: every send delivers exactly once
// or surfaces a visible degraded state — silence is a failure.

/// Tests that flip the process-global OS-path flag or assert precise dial
/// timing serialize through this: a park triggered by one test inflates
/// another's measured backoff gaps.
static PATH_AND_TIMING: Mutex<()> = Mutex::new(());

struct FlakyConnector {
    /// One entry per dial: `Some(pipe)` connects, `None` refuses.
    script: Mutex<VecDeque<Option<BinPipe>>>,
    /// Virtual-clock instant of every dial attempt (gap assertions).
    times: Mutex<Vec<tokio::time::Instant>>,
}

impl FlakyConnector {
    fn new(script: Vec<Option<BinPipe>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into_iter().collect()),
            times: Mutex::new(Vec::new()),
        })
    }
    fn dial_times(&self) -> Vec<tokio::time::Instant> {
        lock(&self.times).clone()
    }
}

impl BinConnector for FlakyConnector {
    fn connect(&self) -> BoxFuture<'static, Result<BinPipe, SyncError>> {
        lock(&self.times).push(tokio::time::Instant::now());
        let slot = lock(&self.script).pop_front().flatten();
        Box::pin(async move { slot.ok_or(SyncError::Closed) })
    }
}

fn empty_state_json() -> serde_json::Value {
    serde_json::json!({"headSeq": 0, "seqFloor": 0,
        "checkpointSeq": 0, "checkpointSize": 0, "rowCount": 0, "rowBytes": 0})
}

/// A network that drops the socket mid-push and then refuses two dials must
/// still deliver the batch EXACTLY once (same batch id, one ack, cursor 1)
/// and must narrate the outage (Disconnected event + disconnect counters) —
/// the UI-truth signal the pill and Queued badges ride on.
#[tokio::test(start_paused = true)]
async fn drops_and_refused_dials_deliver_the_push_exactly_once() {
    let _serial = lock(&PATH_AND_TIMING);
    let (pipe1, mut end1) = pipe_pair();
    let (pipe2, mut end2) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");
    // Dial script: connect, then two refusals (the flap), then recover.
    let flaky = FlakyConnector::new(vec![Some(pipe1), None, None, Some(pipe2)]);

    let s1 = tokio::spawn({
        let state = empty_state_json();
        async move {
            serve_join(&mut end1, state, &[], vec![], false).await;
            let push = expect_kind(&mut end1, frame_type::PUSH).await;
            let batch_id = push.header["batchId"].as_str().unwrap().to_string();
            drop(end1); // mid-push socket death: no ack
            batch_id
        }
    });

    let client = ChatClient::connect_with_tuned(
        flaky.clone(),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("first join succeeds");
    let mut events = client.events();

    client.enqueue_update(vec![0xEE]);
    let first_batch = s1.await.unwrap();

    // The outage narrates: a Disconnected event reaches subscribers.
    let mut saw_disconnect = false;
    // Serve the recovery session concurrently so the actor can get there.
    let s2 = tokio::spawn({
        let state = empty_state_json();
        async move {
            serve_join(&mut end2, state, &[], vec![], true).await;
            let push = expect_kind(&mut end2, frame_type::PUSH).await;
            let batch_id = push.header["batchId"].as_str().unwrap().to_string();
            send(
                &end2,
                frame_type::ACK,
                serde_json::json!({"batchId": batch_id, "seq": 1, "dup": false}),
                &[],
            )
            .await;
            (batch_id, end2)
        }
    });
    let (replayed, _keep_alive) = s2.await.unwrap();
    assert_eq!(
        replayed, first_batch,
        "the SAME batch replays — no dupes, no loss"
    );

    while client.stats().pending_pushes > 0 {
        if matches!(events.recv().await, Ok(ChatEvent::Disconnected)) {
            saw_disconnect = true;
        }
    }
    while let Ok(event) = events.try_recv() {
        if matches!(event, ChatEvent::Disconnected) {
            saw_disconnect = true;
        }
    }
    assert!(
        saw_disconnect,
        "the outage must surface as a Disconnected event"
    );
    let stats = client.stats();
    assert_eq!(stats.cursor, 1, "exactly one ack advanced the cursor");
    assert_eq!(*lock(&sink.cursor_advances), vec![1]);
    assert!(
        stats.disconnects >= 1,
        "disconnect counter recorded the drop"
    );
    assert_eq!(
        flaky.dial_times().len(),
        4,
        "exactly the scripted dials: join, two refusals, recovery"
    );
    client.shutdown().await;
}

/// Stability-gated backoff reset: sessions that JOIN and then die immediately
/// (captive portal / connect-and-die socket) must keep growing their backoff
/// — reset-on-join alone hot-looped at 250ms forever. A session that stays
/// healthy past STABLE_RESET earns the fresh 250ms base again.
#[tokio::test(start_paused = true)]
async fn connect_and_die_sessions_grow_backoff_until_a_stable_session_resets_it() {
    let _serial = lock(&PATH_AND_TIMING);
    let mut pipes = Vec::new();
    let mut ends = VecDeque::new();
    for _ in 0..5 {
        let (pipe, end) = pipe_pair();
        pipes.push(Some(pipe));
        ends.push_back(end);
    }
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");
    let flaky = FlakyConnector::new(pipes);

    // Sessions 1-4: join then die instantly. Session 5: join, stay healthy
    // past STABLE_RESET, then die.
    let server = tokio::spawn(async move {
        for i in 0..4 {
            let mut end = ends.pop_front().unwrap();
            serve_join(&mut end, empty_state_json(), &[], vec![], i > 0).await;
            drop(end); // joined-and-died: an unstable session
        }
        let mut end = ends.pop_front().unwrap();
        serve_join(&mut end, empty_state_json(), &[], vec![], true).await;
        // Outlive the stability gate on the virtual clock, then die.
        tokio::time::sleep(STABLE_RESET + Duration::from_secs(1)).await;
        drop(end);
    });

    let client = ChatClient::connect_with_tuned(
        flaky.clone(),
        sink,
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("first join succeeds");

    // Wait until all five scripted sessions have been dialed (the 6th dial
    // attempt hits the exhausted script and keeps failing — ignore it).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    while flaky.dial_times().len() < 6 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "dials never happened"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.await.unwrap();
    let times = flaky.dial_times();
    // Gaps between redials after unstable sessions must GROW (each joined
    // session died in ~0 virtual time, so the gap is ~the backoff).
    let gap = |i: usize| times[i + 1].duration_since(times[i]);
    assert!(
        gap(1) >= Duration::from_millis(400),
        "second redial backed off: {:?}",
        gap(1)
    );
    assert!(
        gap(2) > gap(1),
        "backoff grows across unstable sessions: {:?} vs {:?}",
        gap(2),
        gap(1)
    );
    assert!(
        gap(3) > gap(2),
        "…and keeps growing: {:?} vs {:?}",
        gap(3),
        gap(2)
    );
    // The stable session (index 4→5 gap includes its >30s healthy lifetime):
    // after it died, the backoff was reset to base — the redial came within
    // ~base of the death, i.e. the whole gap is dominated by the 31s life.
    let stable_gap = gap(4);
    assert!(
        stable_gap < STABLE_RESET + Duration::from_secs(3),
        "a stable session resets the backoff to base: {stable_gap:?}"
    );
    client.shutdown().await;
}

/// Park-while-offline: while the OS reports no network path, the backoff
/// waiter parks on the event buses instead of burning dial attempts — and
/// the online event un-parks it IMMEDIATELY (event-driven recovery, not
/// timer luck).
#[tokio::test(start_paused = true)]
async fn os_offline_parks_dials_and_the_online_event_unparks_immediately() {
    let _serial = lock(&PATH_AND_TIMING);
    let (pipe1, mut end1) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");
    let flaky = FlakyConnector::new(vec![Some(pipe1)]);

    let server = tokio::spawn(async move {
        serve_join(&mut end1, empty_state_json(), &[], vec![], false).await;
        // Hold the session; the path flag flips BEFORE this socket dies, so
        // the redial wait is born parked.
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(end1); // the network goes away
    });

    let client = ChatClient::connect_with_tuned(
        flaky.clone(),
        sink,
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");

    // The OS says the path is gone while the session is still up.
    crate::wake::set_path_online(false);
    server.await.unwrap();
    // Give the actor a beat to observe the death and enter the parked wait.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let dials_before = flaky.dial_times().len();

    // 5 virtual seconds pass — far beyond the normal 250-500ms backoff. A
    // parked waiter must NOT have dialed (the un-parked pre-fix behavior
    // would have burned several attempts by now).
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        flaky.dial_times().len(),
        dials_before,
        "no dial attempts while the OS says there is no path"
    );

    // The path returns: the transition broadcasts the online event and the
    // parked waiter redials NOW.
    let restored_at = tokio::time::Instant::now();
    crate::wake::set_path_online(true);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while flaky.dial_times().len() == dials_before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "online event never un-parked the dialer"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let redial_at = *flaky.dial_times().last().unwrap();
    assert!(
        redial_at.duration_since(restored_at) < Duration::from_secs(2),
        "redial rides the event, not a timer: {:?}",
        redial_at.duration_since(restored_at)
    );
    client.shutdown().await;
}

// ── row-gap contiguity + repair (2026-08-19 empty-doc/advanced-cursor wedge) ─

/// A live broadcast can outrun the backfill during a join, delivering seq N
/// while seq N-1 was never received. The old `cursor.max(seq)` advance
/// stamped the cursor over the hole — the skipped row's dependents parked
/// invisibly in loro's pending buffer and the doc read empty forever while
/// the client polled `after=cursor` ("new session hangs, retry no-op"). The
/// cursor must HOLD at the last contiguous seq and a backfill repair must
/// close the gap.
#[tokio::test(start_paused = true)]
async fn live_row_gap_holds_cursor_and_repairs() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");

    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 1, "seqFloor": 0, "checkpointSeq": 0,
                "checkpointSize": 0, "rowCount": 1, "rowBytes": 32}),
            &[],
            vec![(1, "dev-b", vec![0x01])],
            false,
        )
        .await;
        assert_eq!(after, 0);
        // Live frame with a HOLE: seq 3 arrives, seq 2 never did.
        send(
            &end,
            frame_type::ROW,
            serde_json::json!({"seq": 3, "device": "dev-b", "batchId": "b3"}),
            &[0x03],
        )
        .await;
        // The client must answer with a backfill request from its HELD
        // cursor (1) — not silently skip to 3.
        let req = expect_kind(&mut end, frame_type::ROWS_REQ).await;
        assert_eq!(
            req.header["after"].as_u64().unwrap(),
            1,
            "repair starts at the honest cursor"
        );
        for (seq, bytes) in [(2u64, vec![0x02u8]), (3, vec![0x03])] {
            send(
                &end,
                frame_type::ROW,
                serde_json::json!({"seq": seq, "device": "dev-b", "batchId": format!("b{seq}")}),
                &bytes,
            )
            .await;
        }
        send(
            &end,
            frame_type::ROWS_DONE,
            serde_json::json!({"headSeq": 3}),
            &[],
        )
        .await;
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    let end = server.await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while client.stats().cursor != 3 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "gap repair never converged the cursor: {:?}",
            client.stats()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The gap row applied with the HELD cursor (1), never with 3; the
    // repair rows then walked it up contiguously.
    assert_eq!(
        *lock(&sink.rows),
        vec![
            (vec![0x01], 1),
            (vec![0x03], 1),
            (vec![0x02], 2),
            (vec![0x03], 3),
        ],
        "cursor held through the gap and walked by the repair"
    );
    drop(end);
    client.shutdown().await;
}

/// A persisted cursor over a CHECKPOINT-LESS room gets amnesty to zero on
/// the first join: if any past import parked (the wedge left on disk by the
/// pre-fix race), the full refetch materializes it — bounded by the
/// checkpoint threshold policy, and re-imports are no-ops.
#[tokio::test(start_paused = true)]
async fn checkpointless_amnesty_refetches_from_zero() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");

    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 3, "seqFloor": 0, "checkpointSeq": 0,
                "checkpointSize": 0, "rowCount": 3, "rowBytes": 96}),
            &[],
            vec![
                (1, "dev-b", vec![0x01]),
                (2, "dev-b", vec![0x02]),
                (3, "dev-b", vec![0x03]),
            ],
            false,
        )
        .await;
        assert_eq!(after, 0, "amnesty must refetch the whole log");
        end
    });

    // Persisted cursor 3 — the on-disk wedge shape (doc may have parked).
    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        3,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    let end = server.await.unwrap();

    assert_eq!(
        *lock(&sink.rows),
        vec![(vec![0x01], 1), (vec![0x02], 2), (vec![0x03], 3)],
        "all rows re-imported from zero"
    );
    assert_eq!(client.stats().cursor, 3);
    drop(end);
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn causal_gap_refetches_checkpoint_without_skipping_parked_row() {
    use std::sync::atomic::Ordering::Relaxed;
    let (first_pipe, mut first_end) = pipe_pair();
    let (second_pipe, mut second_end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    sink.frontier_contained.store(true, Relaxed);
    sink.pending_until_checkpoint.store(true, Relaxed);
    let (fetch, calls) = fetcher(b"complete-history");
    let server = tokio::spawn(async move {
        let state = serde_json::json!({"headSeq": 5, "seqFloor": 5,
            "checkpointSeq": 5, "checkpointSize": 1000, "rowCount": 0, "rowBytes": 0});
        serve_join(&mut first_end, state.clone(), b"frontier", vec![], false).await;
        send(
            &first_end,
            frame_type::ROW,
            serde_json::json!({"seq": 6, "device": "dev-b", "batchId": "b6"}),
            b"dependent-row",
        )
        .await;
        let hello = expect_kind(&mut second_end, frame_type::HELLO).await;
        assert_eq!(
            hello.header["cursor"], 5,
            "parked row must not advance cursor"
        );
        let mut state = state;
        state["headSeq"] = 6.into();
        send(&second_end, frame_type::STATE, state, b"frontier").await;
        let rows = expect_kind(&mut second_end, frame_type::ROWS_REQ).await;
        assert_eq!(rows.header["after"], 5);
        assert_eq!(
            rows.header["excludeOwn"], false,
            "repair includes every writer"
        );
        send(
            &second_end,
            frame_type::ROW,
            serde_json::json!({"seq": 6, "device": "dev-b", "batchId": "b6"}),
            b"dependent-row",
        )
        .await;
        send(
            &second_end,
            frame_type::ROWS_DONE,
            serde_json::json!({"headSeq": 6}),
            &[],
        )
        .await;
        (first_end, second_end)
    });
    let client = ChatClient::connect_with_tuned(
        connector(vec![first_pipe, second_pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        5,
        ChatTuning::default(),
    )
    .await
    .unwrap();
    let ends = server.await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while client.stats().cursor != 6 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "causal repair stalled"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        calls.load(Relaxed),
        1,
        "fetch despite the contained frontier"
    );
    assert_eq!(*lock(&sink.ops), vec!["ckpt@5", "row@6"]);
    drop(ends);
    client.shutdown().await;
}

struct FixedHttpRows {
    body: Vec<u8>,
    requests: Mutex<Vec<u64>>,
}
impl ChatTransport for FixedHttpRows {
    fn fetch_rows(&self, after: u64) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        lock(&self.requests).push(after);
        let body = self.body.clone();
        Box::pin(async move { Ok(body) })
    }
    fn push(&self, _: String, _: Vec<u8>) -> BoxFuture<'static, Result<String, SyncError>> {
        Box::pin(async { panic!("read-only recovery must not push") })
    }
}

#[tokio::test(start_paused = true)]
async fn http_catchup_crosses_a_contained_checkpoint_and_repairs_causal_gaps() {
    use std::sync::atomic::Ordering::Relaxed;
    for pending_dependencies in [false, true] {
        let sink = Arc::new(RecordingSink::default());
        sink.frontier_contained.store(true, Relaxed);
        sink.pending_until_checkpoint
            .store(pending_dependencies, Relaxed);
        let (fetch, calls) = fetcher(b"complete-history");
        let frames = [
            encode(
                frame_type::STATE,
                &serde_json::json!({"headSeq": 6, "seqFloor": 5,
                "checkpointSeq": 5, "checkpointSize": 1000, "rowCount": 1, "rowBytes": 20}),
                b"frontier",
            ),
            encode(
                frame_type::ROW,
                &serde_json::json!({"seq": 6, "device": "dev-b", "batchId": "b6"}),
                b"row",
            ),
            encode(
                frame_type::ROWS_DONE,
                &serde_json::json!({"headSeq": 6}),
                &[],
            ),
        ];
        let mut body = Vec::new();
        for frame in frames {
            body.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            body.extend_from_slice(&frame);
        }
        let transport = Arc::new(FixedHttpRows {
            body,
            requests: Mutex::new(Vec::new()),
        });
        let client = ChatClient::connect_with_transport(
            connector(vec![]),
            sink.clone(),
            fetch,
            "dev-a",
            1,
            ChatTuning::default(),
            Some(transport.clone()),
        )
        .await
        .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while client.stats().cursor != 6 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "HTTP recovery stalled"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(calls.load(Relaxed), u64::from(pending_dependencies));
        assert_eq!(lock(&transport.requests)[0], 1);
        assert_eq!(lock(&sink.rows).last().unwrap().1, 6);
        if pending_dependencies {
            assert_eq!(
                lock(&transport.requests)[1],
                5,
                "retry starts before the parked row"
            );
            assert_eq!(lock(&sink.ops)[0], "ckpt@5");
        }
        client.shutdown().await;
    }
}
