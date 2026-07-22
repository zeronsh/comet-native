//! Fake-transport unit tests: an in-memory duplex (`mpsc` pipes) to a
//! `FakeEdge` that mirrors `edge/src/session-room.ts` semantics — join with VV
//! backfill, DocUpdate import + Ack + broadcast, fragmentation above the
//! payload budget, and injectable InvalidUpdate acks for the stale-peer path.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition not reached in time");
}

struct FakeConn {
    tx: mpsc::Sender<Vec<u8>>,
    task: tokio::task::JoinHandle<()>,
}

struct FakeEdge {
    doc: LoroDoc,
    eph: EphemeralStore,
    conns: Mutex<Vec<FakeConn>>,
    fragments: Mutex<HashMap<BatchId, FragmentBuffer>>,
    /// When set, the next %LOR DocUpdate is rejected with InvalidUpdate
    /// without being imported (simulates the shallow-trim stale-peer case).
    reject_next_update: AtomicBool,
    leaves: AtomicUsize,
    join_requests: AtomicUsize,
}

impl FakeEdge {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            doc: LoroDoc::new(),
            eph: EphemeralStore::new(30_000),
            conns: Mutex::new(Vec::new()),
            fragments: Mutex::new(HashMap::new()),
            reject_next_update: AtomicBool::new(false),
            leaves: AtomicUsize::new(0),
            join_requests: AtomicUsize::new(0),
        })
    }

    fn connector(self: &Arc<Self>) -> Arc<dyn Connector> {
        Arc::new(FakeConnector { edge: self.clone() })
    }

    /// Kill every live connection (client observes an abrupt close).
    fn kick_all(&self) {
        for conn in self.conns.lock().unwrap().drain(..) {
            conn.task.abort();
            drop(conn.tx);
        }
    }

    async fn reply(&self, to: &mpsc::Sender<Vec<u8>>, message: &ProtocolMessage) {
        let _ = to.send(encode(message).expect("encode")).await;
    }

    /// Mirror of the edge's `sendUpdates`: fragment any single update above
    /// the payload budget.
    async fn send_updates(
        &self,
        to: &mpsc::Sender<Vec<u8>>,
        crdt: CrdtType,
        room_id: &str,
        update: Vec<u8>,
    ) {
        if update.len() <= FRAGMENT_BYTES {
            self.reply(
                to,
                &ProtocolMessage::DocUpdate {
                    crdt,
                    room_id: room_id.to_string(),
                    updates: vec![update],
                    batch_id: new_batch_id(),
                },
            )
            .await;
            return;
        }
        let batch_id = new_batch_id();
        self.reply(
            to,
            &ProtocolMessage::DocUpdateFragmentHeader {
                crdt,
                room_id: room_id.to_string(),
                batch_id,
                fragment_count: update.len().div_ceil(FRAGMENT_BYTES) as u64,
                total_size_bytes: update.len() as u64,
            },
        )
        .await;
        for (index, chunk) in update.chunks(FRAGMENT_BYTES).enumerate() {
            self.reply(
                to,
                &ProtocolMessage::DocUpdateFragment {
                    crdt,
                    room_id: room_id.to_string(),
                    batch_id,
                    index: index as u64,
                    fragment: chunk.to_vec(),
                },
            )
            .await;
        }
    }

    async fn handle(&self, reply_to: &mpsc::Sender<Vec<u8>>, bytes: &[u8]) {
        let message = decode(bytes).expect("client sent an undecodable frame");
        match message {
            ProtocolMessage::JoinRequest {
                crdt: CrdtType::Loro,
                room_id,
                version,
                ..
            } => {
                self.join_requests.fetch_add(1, Ordering::SeqCst);
                self.reply(
                    reply_to,
                    &ProtocolMessage::JoinResponseOk {
                        crdt: CrdtType::Loro,
                        room_id: room_id.clone(),
                        permission: Permission::Write,
                        version: self.doc.oplog_vv().encode(),
                        extra: None,
                    },
                )
                .await;
                let backfill = if version.is_empty() {
                    self.doc.export(ExportMode::Snapshot)
                } else {
                    match VersionVector::decode(&version) {
                        Ok(vv) => self.doc.export(ExportMode::updates(&vv)),
                        Err(_) => self.doc.export(ExportMode::Snapshot),
                    }
                }
                .expect("export backfill");
                if !backfill.is_empty() {
                    self.send_updates(reply_to, CrdtType::Loro, &room_id, backfill)
                        .await;
                }
            }
            ProtocolMessage::JoinRequest {
                crdt: CrdtType::LoroEphemeralStore,
                room_id,
                ..
            } => {
                self.reply(
                    reply_to,
                    &ProtocolMessage::JoinResponseOk {
                        crdt: CrdtType::LoroEphemeralStore,
                        room_id: room_id.clone(),
                        permission: Permission::Write,
                        version: Vec::new(),
                        extra: None,
                    },
                )
                .await;
                let all = self.eph.encode_all();
                if !all.is_empty() {
                    self.send_updates(reply_to, CrdtType::LoroEphemeralStore, &room_id, all)
                        .await;
                }
            }
            ProtocolMessage::DocUpdate {
                crdt,
                room_id,
                updates,
                batch_id,
            } => {
                self.apply(reply_to, crdt, &room_id, batch_id, updates)
                    .await;
            }
            ProtocolMessage::DocUpdateFragmentHeader {
                crdt,
                batch_id,
                fragment_count,
                total_size_bytes,
                ..
            } => {
                self.fragments.lock().unwrap().insert(
                    batch_id,
                    FragmentBuffer {
                        crdt,
                        parts: vec![None; fragment_count as usize],
                        received: 0,
                        total_size: total_size_bytes as usize,
                    },
                );
            }
            ProtocolMessage::DocUpdateFragment {
                crdt,
                room_id,
                batch_id,
                index,
                fragment,
            } => {
                enum Outcome {
                    /// Header lost (hibernation analogue) — FragmentTimeout.
                    Timeout,
                    Incomplete,
                    Complete(Vec<u8>),
                }
                let outcome = {
                    let mut fragments = self.fragments.lock().unwrap();
                    match fragments.get_mut(&batch_id) {
                        None => Outcome::Timeout,
                        Some(buffer) => {
                            if buffer.parts[index as usize].is_none() {
                                buffer.received += 1;
                            }
                            buffer.parts[index as usize] = Some(fragment);
                            if buffer.received < buffer.parts.len() {
                                Outcome::Incomplete
                            } else {
                                let buffer = fragments.remove(&batch_id).unwrap();
                                let mut total = Vec::with_capacity(buffer.total_size);
                                for part in buffer.parts.into_iter().flatten() {
                                    total.extend_from_slice(&part);
                                }
                                Outcome::Complete(total)
                            }
                        }
                    }
                };
                match outcome {
                    Outcome::Timeout => {
                        self.reply(
                            reply_to,
                            &ProtocolMessage::Ack {
                                crdt,
                                room_id,
                                ref_id: batch_id,
                                status: UpdateStatusCode::FragmentTimeout,
                            },
                        )
                        .await;
                    }
                    Outcome::Incomplete => {}
                    Outcome::Complete(total) => {
                        self.apply(reply_to, crdt, &room_id, batch_id, vec![total])
                            .await;
                    }
                }
            }
            ProtocolMessage::Leave { .. } => {
                self.leaves.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }

    /// Mirror of the edge's `applyUpdates`: import, ack, broadcast to peers.
    async fn apply(
        &self,
        reply_to: &mpsc::Sender<Vec<u8>>,
        crdt: CrdtType,
        room_id: &str,
        batch_id: BatchId,
        updates: Vec<Vec<u8>>,
    ) {
        let ack = |status| ProtocolMessage::Ack {
            crdt,
            room_id: room_id.to_string(),
            ref_id: batch_id,
            status,
        };
        if crdt == CrdtType::Loro && self.reject_next_update.swap(false, Ordering::SeqCst) {
            self.reply(reply_to, &ack(UpdateStatusCode::InvalidUpdate))
                .await;
            return;
        }
        let ok = match crdt {
            CrdtType::Loro => updates
                .iter()
                .filter(|u| !u.is_empty())
                .all(|u| self.doc.import(u).is_ok()),
            CrdtType::LoroEphemeralStore => updates
                .iter()
                .filter(|u| !u.is_empty())
                .all(|u| self.eph.apply(u).is_ok()),
            _ => false,
        };
        if !ok {
            self.reply(reply_to, &ack(UpdateStatusCode::InvalidUpdate))
                .await;
            return;
        }
        self.reply(reply_to, &ack(UpdateStatusCode::Ok)).await;
        // Broadcast to every other live connection (edge excludes the sender).
        let peers: Vec<mpsc::Sender<Vec<u8>>> = self
            .conns
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.tx.clone())
            .filter(|tx| !tx.same_channel(reply_to))
            .collect();
        for peer in peers {
            for update in &updates {
                self.send_updates(&peer, crdt, room_id, update.clone())
                    .await;
            }
        }
    }
}

struct FakeConnector {
    edge: Arc<FakeEdge>,
}

impl Connector for FakeConnector {
    fn connect(&self) -> BoxFuture<'static, Result<Pipe, SyncError>> {
        let edge = self.edge.clone();
        Box::pin(async move {
            let (client_tx, mut server_rx) = mpsc::channel::<Vec<u8>>(256);
            let (server_tx, client_rx) = mpsc::channel::<Vec<u8>>(256);
            let reply_to = server_tx.clone();
            let handler_edge = edge.clone();
            let task = tokio::spawn(async move {
                while let Some(bytes) = server_rx.recv().await {
                    handler_edge.handle(&reply_to, &bytes).await;
                }
            });
            edge.conns.lock().unwrap().push(FakeConn {
                tx: server_tx,
                task,
            });
            Ok(Pipe {
                tx: client_tx,
                rx: client_rx,
            })
        })
    }
}

fn doc_text(doc: &LoroDoc) -> String {
    doc.get_text("t").to_string()
}

#[tokio::test]
async fn join_backfills_server_state_into_fresh_doc() {
    let edge = FakeEdge::new();
    edge.doc.get_text("t").insert(0, "server state").unwrap();
    edge.doc.commit();

    let doc = LoroDoc::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("connect");
    wait_until(|| doc_text(&doc) == "server state").await;
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn join_pushes_local_history_the_server_lacks() {
    let edge = FakeEdge::new();
    let doc = LoroDoc::new();
    doc.get_text("t").insert(0, "local first").unwrap();
    doc.commit();

    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("connect");
    wait_until(|| doc_text(&edge.doc) == "local first").await;
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn local_updates_push_ack_and_broadcast_to_peer() {
    let edge = FakeEdge::new();
    let doc_a = LoroDoc::new();
    let doc_b = LoroDoc::new();
    let a = RoomClient::connect_with(edge.connector(), "room-1", doc_a.clone())
        .await
        .expect("connect a");
    let b = RoomClient::connect_with(edge.connector(), "room-1", doc_b.clone())
        .await
        .expect("connect b");

    doc_a.get_text("t").insert(0, "hello from a").unwrap();
    doc_a.commit();

    wait_until(|| doc_text(&edge.doc) == "hello from a").await;
    wait_until(|| doc_text(&doc_b) == "hello from a").await;
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_update_ack_triggers_rejoin_and_resubmit_from_vv() {
    let edge = FakeEdge::new();
    let doc = LoroDoc::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("connect");

    let joins_before = edge.join_requests.load(Ordering::SeqCst);
    edge.reject_next_update.store(true, Ordering::SeqCst);
    doc.get_text("t").insert(0, "retried write").unwrap();
    doc.commit();

    // The edge rejected the first submission without importing; the client
    // must rejoin (resync) and resubmit from the server's VV until converged.
    wait_until(|| doc_text(&edge.doc) == "retried write").await;
    assert!(
        edge.join_requests.load(Ordering::SeqCst) > joins_before,
        "client must rejoin"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn large_updates_fragment_out_and_reassemble_in() {
    let edge = FakeEdge::new();
    let doc_a = LoroDoc::new();
    let doc_b = LoroDoc::new();
    let a = RoomClient::connect_with(edge.connector(), "room-1", doc_a.clone())
        .await
        .expect("connect a");
    let b = RoomClient::connect_with(edge.connector(), "room-1", doc_b.clone())
        .await
        .expect("connect b");

    // Well above FRAGMENT_BYTES: A's push fragments client→server, and the
    // broadcast to B fragments server→client (reassembly both directions).
    let big = "x".repeat(3 * FRAGMENT_BYTES + 12345);
    doc_a.get_text("t").insert(0, &big).unwrap();
    doc_a.commit();

    wait_until(|| doc_text(&edge.doc) == big).await;
    wait_until(|| doc_text(&doc_b) == big).await;
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn ephemeral_presence_relays_between_peers() {
    let edge = FakeEdge::new();
    let a = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect a");
    let b = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect b");

    a.ephemeral().set("device:a", "online");
    wait_until(|| b.ephemeral().get("device:a") == Some("online".into())).await;

    // Late joiner receives the server's accumulated presence on join.
    let c = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect c");
    wait_until(|| c.ephemeral().get("device:a") == Some("online".into())).await;

    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    c.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconnects_with_backoff_and_rejoins_after_connection_loss() {
    let edge = FakeEdge::new();
    let doc = LoroDoc::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("connect");
    let mut events = client.events();

    edge.kick_all();
    // Write while disconnected: must arrive after the automatic rejoin via the
    // join-time VV diff.
    doc.get_text("t").insert(0, "written offline").unwrap();
    doc.commit();

    wait_until(|| doc_text(&edge.doc) == "written offline").await;

    // Lifecycle events observed: a disconnect, then a (re)connect.
    let mut saw_disconnect = false;
    let mut saw_reconnect = false;
    while let Ok(event) = events.try_recv() {
        match event {
            RoomEvent::Disconnected => saw_disconnect = true,
            RoomEvent::Connected if saw_disconnect => saw_reconnect = true,
            _ => {}
        }
    }
    assert!(
        saw_disconnect && saw_reconnect,
        "expected Disconnected then Connected"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_sends_leave() {
    let edge = FakeEdge::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect");
    client.shutdown().await.unwrap();
    wait_until(|| edge.leaves.load(Ordering::SeqCst) >= 1).await;
}

/// The per-dial URL provider seam: a signed-out provider fails the connect
/// fast with `SyncError::Auth` (no socket is ever attempted).
#[tokio::test]
async fn connect_via_surfaces_url_provider_auth_error() {
    struct SignedOut;
    impl UrlProvider for SignedOut {
        fn url(&self) -> BoxFuture<'static, Result<String, SyncError>> {
            Box::pin(async { Err(SyncError::Auth("signed out".into())) })
        }
    }
    let result = RoomClient::connect_via(Arc::new(SignedOut), "room-1", LoroDoc::new()).await;
    match result {
        Ok(_) => panic!("connect must fail"),
        Err(err) => assert!(matches!(err, SyncError::Auth(_)), "got: {err}"),
    }
}

#[tokio::test]
async fn first_connect_failure_is_returned() {
    struct FailingConnector;
    impl Connector for FailingConnector {
        fn connect(&self) -> BoxFuture<'static, Result<Pipe, SyncError>> {
            Box::pin(async { Err(SyncError::WebSocket("refused".into())) })
        }
    }
    let result =
        RoomClient::connect_with(Arc::new(FailingConnector), "room-1", LoroDoc::new()).await;
    match result {
        Ok(_) => panic!("connect must fail"),
        Err(err) => assert!(matches!(err, SyncError::WebSocket(_))),
    }
}
