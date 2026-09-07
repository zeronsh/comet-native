//! chat2 host wiring (docs/chat2-sync.md C3): the engine-side implementations
//! of [`zeron_sync::chat_client::ChatDocSink`] and
//! [`zeron_sync::chat_client::CheckpointFetcher`], binding a
//! [`crate::doc_host::ChatDocHandle`]'s live doc to a chat2 room.
//!
//! The C2 rule is enforced HERE: every sink method persists doc content AND
//! the room cursor in one `save_snapshot_with_cursor` transaction, so a
//! restored backup can never disagree with its own cursor — the root cause
//! of the redownload-forever class the old s2 clients suffered.
//!
//! Encrypted profiles (RFC 0001 §8) add a [`ChatCodec`] in front of every
//! import and behind every export: rows, checkpoints, and frontiers are
//! signed+sealed content records opened against the vault's pinned
//! membership before Loro sees a byte, and local updates are sealed once
//! into the durable outbox (`DocsStore::persist_encrypted_batch`) before any
//! transport carries them. There is no plaintext fallback: an encrypted
//! room whose keys are missing pauses, it never downgrades.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use zeron_crypto::content::{self, ContentPurpose};
use zeron_crypto::record::{RecordError, UnverifiedRecord};
use zeron_doc::SessionDoc;
use zeron_sync::chat_client::{ApplyOutcome, ChatDocSink, CheckpointFetcher, MAX_PUSH_BYTES};
use zeron_sync::{DocsStore, PendingEncryptedBatch, SyncError};

use crate::EngineError;
use crate::doc_host::EdgeConfig;
use crate::vault::{OpenFailure, VaultService};

/// Doc epoch stamped on every chat2-synced snapshot (docs/chat2-sync.md M1:
/// thin docs are lineage epoch 2; M3 readers discard-and-adopt below it).
pub const CHAT2_DOC_EPOCH: u32 = 2;
/// Doc epoch of a snapshot whose cursor belongs to the ENCRYPTED room
/// (RFC 0001 §12: a fresh storage generation per encrypted profile). A
/// stored cursor from a lower epoch names the plaintext room and is not
/// carried over.
pub const CHAT2_ENCRYPTED_DOC_EPOCH: u32 = 3;
/// Content bytes a sealed row may carry: the transport cap minus the signed
/// wrapper + encrypted-payload overhead (`content::PAYLOAD_OVERHEAD` 144 +
/// `record::MAX_OVERHEAD` 256), rounded down for headroom.
pub const MAX_SEALED_UPDATE_BYTES: usize = MAX_PUSH_BYTES - 512;
/// Sealed frontier budget (a Loro version vector is small; this is a bound,
/// not a target).
const MAX_FRONTIER_BYTES: usize = 64 * 1024;

/// The room an encrypted profile's chat lives in: a distinct namespace from
/// the plaintext room so ciphertext and legacy rows never share a log, and
/// so the plaintext copy is retained (never silently deleted) for the
/// separate cleanup step. Stays within the edge's `[A-Za-z0-9_-]{1,128}`.
pub fn encrypted_room_id(chat_id: &str) -> String {
    const SUFFIX: &str = "-e1";
    if chat_id.len() + SUFFIX.len() <= 128 {
        format!("{chat_id}{SUFFIX}")
    } else {
        let digest = zeron_crypto::sha256(&[b"zeron/encrypted-room/v1\0", chat_id.as_bytes()]);
        let hex: String = digest[..20].iter().map(|b| format!("{b:02x}")).collect();
        format!("e1-{hex}")
    }
}

/// Seal/open boundary for one chat object (RFC 0001 §7.7, §8).
#[derive(Clone)]
pub struct ChatCodec {
    vault: VaultService,
    object_id: [u8; 16],
}

impl ChatCodec {
    pub fn new(vault: VaultService, chat_id: &str) -> Self {
        Self {
            object_id: crate::vault::object_id_for("chat", chat_id),
            vault,
        }
    }

    pub fn object_id(&self) -> [u8; 16] {
        self.object_id
    }

    pub fn vault(&self) -> &VaultService {
        &self.vault
    }

    /// Seal `plaintext` under the current epoch (publishing this object's
    /// key first when needed). Sealing is async because the object key may
    /// have to become durable on the control plane before first use.
    pub async fn seal(
        &self,
        purpose: ContentPurpose,
        plaintext: &[u8],
        max_plaintext_bytes: usize,
    ) -> Result<content::SealedContent, EngineError> {
        let material = self.vault.seal_material(self.object_id).await?;
        content::seal(
            &material.binding,
            purpose,
            &material.key,
            &material.signer,
            plaintext,
            max_plaintext_bytes,
        )
        .map_err(|e| EngineError::Other(format!("seal: {e}")))
    }

    /// Open a record with cached keys only. `Err(outcome)` is the explicit
    /// verified result for the cursor; `KeyUnavailable` also kicks a
    /// background key refresh so the host can `resume` afterwards.
    pub fn open(
        &self,
        purpose: ContentPurpose,
        encoded: &[u8],
        max_plaintext_bytes: usize,
    ) -> Result<Vec<u8>, ApplyOutcome> {
        let payload_limit = max_plaintext_bytes.saturating_add(144);
        let parsed = UnverifiedRecord::parse(encoded, payload_limit).map_err(|err| match err {
            RecordError::UnsupportedVersion | RecordError::UnsupportedKind => {
                ApplyOutcome::Unsupported
            }
            _ => ApplyOutcome::AuthenticationFailed,
        })?;
        let binding = *parsed.untrusted_binding();
        let context = self
            .vault
            .open_material_cached(self.object_id, &binding)
            .map_err(|failure| match failure {
                OpenFailure::Unavailable | OpenFailure::KeyUnavailable => {
                    self.vault.spawn_key_refresh(self.object_id);
                    ApplyOutcome::KeyUnavailable
                }
                OpenFailure::NotAuthorized => ApplyOutcome::AuthenticationFailed,
            })?;
        let opened = content::open(
            encoded,
            &context.binding,
            purpose,
            &context.key,
            &context.author_public_key,
            max_plaintext_bytes,
        )
        .map_err(|err| match err {
            content::ContentError::UnsupportedFormat
            | content::ContentError::UnsupportedSuite
            | content::ContentError::UnsupportedPurpose => ApplyOutcome::Unsupported,
            _ => ApplyOutcome::AuthenticationFailed,
        })?;
        Ok(opened.plaintext().as_bytes().to_vec())
    }
}

impl ChatCodec {
    /// Open with a network fetch of missing object keys (for on-demand
    /// reads outside the cursor path, e.g. blob display).
    pub async fn open_async(
        &self,
        purpose: ContentPurpose,
        encoded: &[u8],
        max_plaintext_bytes: usize,
    ) -> Result<Vec<u8>, ApplyOutcome> {
        let payload_limit = max_plaintext_bytes.saturating_add(144);
        let parsed = UnverifiedRecord::parse(encoded, payload_limit)
            .map_err(|_| ApplyOutcome::AuthenticationFailed)?;
        let binding = *parsed.untrusted_binding();
        let context = self
            .vault
            .open_material(self.object_id, &binding)
            .await
            .map_err(|failure| match failure {
                OpenFailure::Unavailable | OpenFailure::KeyUnavailable => {
                    ApplyOutcome::KeyUnavailable
                }
                OpenFailure::NotAuthorized => ApplyOutcome::AuthenticationFailed,
            })?;
        content::open(
            encoded,
            &context.binding,
            purpose,
            &context.key,
            &context.author_public_key,
            max_plaintext_bytes,
        )
        .map(|opened| opened.plaintext().as_bytes().to_vec())
        .map_err(|_| ApplyOutcome::AuthenticationFailed)
    }
}

type ResumeHook = Arc<dyn Fn() + Send + Sync>;

/// [`ChatDocSink`] over a live [`SessionDoc`] + the cursor-bearing store.
///
/// Loro import of a remote row/checkpoint fires the doc's root subscription,
/// so the transcript watch, command drain, and debounced UI publish all ride
/// the existing change plumbing — this type only owns import + same-tx
/// persistence (and, for encrypted rooms, the seal/open boundary).
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
    codec: Option<ChatCodec>,
    doc_epoch: u32,
    /// Last cursor this sink persisted (the outbox commit reuses it so a
    /// sealed batch's snapshot never regresses the verified cursor).
    last_cursor: AtomicU64,
    /// Outbox receipts by wire batch id, retired on ack/permanent rejection.
    outbox: Mutex<HashMap<String, PendingEncryptedBatch>>,
    /// Installed once the client exists: called when keys arrive so the
    /// paused client backfills from its honest cursor.
    resume: Mutex<Option<ResumeHook>>,
}

impl EngineChatSink {
    pub fn new(doc: &Arc<SessionDoc>, store: Arc<DocsStore>, chat_id: impl Into<String>) -> Self {
        Self {
            doc: Arc::downgrade(doc),
            store,
            chat_id: chat_id.into(),
            codec: None,
            doc_epoch: CHAT2_DOC_EPOCH,
            last_cursor: AtomicU64::new(0),
            outbox: Mutex::new(HashMap::new()),
            resume: Mutex::new(None),
        }
    }

    /// An encrypted-room sink: every import opens through `codec`, every
    /// export seals through it, and snapshots carry the encrypted epoch.
    pub fn encrypted(
        doc: &Arc<SessionDoc>,
        store: Arc<DocsStore>,
        chat_id: impl Into<String>,
        codec: ChatCodec,
        initial_cursor: u64,
    ) -> Self {
        Self {
            doc: Arc::downgrade(doc),
            store,
            chat_id: chat_id.into(),
            codec: Some(codec),
            doc_epoch: CHAT2_ENCRYPTED_DOC_EPOCH,
            last_cursor: AtomicU64::new(initial_cursor),
            outbox: Mutex::new(HashMap::new()),
            resume: Mutex::new(None),
        }
    }

    pub fn codec(&self) -> Option<&ChatCodec> {
        self.codec.as_ref()
    }

    pub fn doc_epoch(&self) -> u32 {
        self.doc_epoch
    }

    pub fn set_resume_hook(&self, hook: ResumeHook) {
        *lock(&self.resume) = Some(hook);
    }

    /// Invoke the resume hook (the host saw keys/policy arrive).
    pub fn resume(&self) {
        if let Some(hook) = lock(&self.resume).clone() {
            hook();
        }
    }

    /// Export the CURRENT doc and persist it with `cursor` in one tx.
    fn persist_with_cursor(&self, cursor: u64) -> Result<(), ()> {
        let Some(doc) = self.doc.upgrade() else {
            return Ok(());
        };
        match doc.export_snapshot() {
            Ok(bytes) => {
                if let Err(err) = self.store.save_snapshot_with_cursor(
                    &self.chat_id,
                    &bytes,
                    cursor,
                    self.doc_epoch,
                ) {
                    tracing::warn!(chat = %self.chat_id, error = %err,
                        "chat2 sink: snapshot persist failed (cursor held; will retry)");
                    return Err(());
                }
                self.last_cursor.fetch_max(cursor, Ordering::Relaxed);
                Ok(())
            }
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err,
                    "chat2 sink: snapshot export failed");
                Err(())
            }
        }
    }

    /// Row bytes as Loro sees them: plaintext rooms pass through; encrypted
    /// rooms open (or report why they cannot).
    fn open_update(&self, bytes: &[u8]) -> Result<Vec<u8>, ApplyOutcome> {
        match &self.codec {
            None => Ok(bytes.to_vec()),
            Some(codec) => codec.open(ContentPurpose::ChatUpdate, bytes, MAX_SEALED_UPDATE_BYTES),
        }
    }

    // ── encrypted outbox ─────────────────────────────────────────────────────

    /// Seal one local update and commit it (with the current snapshot and
    /// verified cursor) to the durable outbox. Returns the wire batch id and
    /// the immutable bytes to enqueue; retries must use exactly these.
    pub async fn seal_and_queue(&self, plaintext: &[u8]) -> Result<(String, Vec<u8>), EngineError> {
        let codec = self
            .codec
            .as_ref()
            .ok_or_else(|| EngineError::Other("plaintext room has no outbox".into()))?;
        if plaintext.len() > MAX_SEALED_UPDATE_BYTES {
            return Err(EngineError::Other(format!(
                "update of {} bytes exceeds the sealed row budget",
                plaintext.len()
            )));
        }
        let sealed = codec
            .seal(
                ContentPurpose::ChatUpdate,
                plaintext,
                MAX_SEALED_UPDATE_BYTES,
            )
            .await?;
        let doc = self
            .doc
            .upgrade()
            .ok_or_else(|| EngineError::Other("doc evicted".into()))?;
        let snapshot = doc
            .export_snapshot()
            .map_err(|e| EngineError::Other(format!("snapshot export: {e}")))?;
        let cursor = self.last_cursor.load(Ordering::Relaxed);
        let receipt = self.store.persist_encrypted_batch(
            &self.chat_id,
            &snapshot,
            cursor,
            self.doc_epoch,
            &sealed,
            zeron_sync::MAX_ENCRYPTED_OUTBOX_BYTES,
        )?;
        let batch_id = batch_id_of(receipt.revision_id());
        let bytes = receipt.encoded().to_vec();
        lock(&self.outbox).insert(batch_id.clone(), receipt);
        Ok((batch_id, bytes))
    }

    /// Every durable batch for this doc, ready to enqueue. Batches sealed
    /// under a superseded policy/epoch are re-sealed under the current one
    /// first (RFC 0001 §11: refresh policy, THEN re-encrypt queued work) and
    /// the stale copy is retired; nothing is ever dropped silently.
    pub async fn replay_outbox(&self) -> Vec<(String, Vec<u8>)> {
        let Some(codec) = self.codec.as_ref() else {
            return Vec::new();
        };
        let (Some(author), Some(public_key)) = (
            codec.vault().device_id(),
            codec.vault().signing_public_key(),
        ) else {
            return Vec::new();
        };
        let current = codec.vault().current_content_binding(codec.object_id());
        let pending = match self.store.pending_encrypted_batches_for_doc(
            &self.chat_id,
            &author,
            &public_key,
            128,
        ) {
            Ok(pending) => pending,
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err,
                    "chat2 outbox: replay read failed; batches retained");
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(pending.len());
        for batch in pending {
            let batch_id = batch_id_of(batch.revision_id());
            if Some(*batch.binding()) == current {
                out.push((batch_id.clone(), batch.encoded().to_vec()));
                lock(&self.outbox).insert(batch_id, batch);
                continue;
            }
            // Stale policy: open our own record with historical keys and
            // seal it again under the current head. Failure keeps the old
            // batch in the outbox for a later attempt.
            let plaintext = match codec.open(
                ContentPurpose::ChatUpdate,
                batch.encoded(),
                MAX_SEALED_UPDATE_BYTES,
            ) {
                Ok(plaintext) => plaintext,
                Err(outcome) => {
                    tracing::warn!(chat = %self.chat_id, ?outcome,
                        "chat2 outbox: stale batch cannot be re-sealed yet; retained");
                    continue;
                }
            };
            match self.seal_and_queue(&plaintext).await {
                Ok((new_id, bytes)) => {
                    if let Err(err) = self.store.acknowledge_encrypted_batch(&batch) {
                        tracing::warn!(chat = %self.chat_id, error = %err,
                            "chat2 outbox: stale batch retire failed");
                    }
                    tracing::info!(chat = %self.chat_id, "chat2 outbox: re-sealed a stale batch");
                    out.push((new_id, bytes));
                }
                Err(err) => {
                    tracing::warn!(chat = %self.chat_id, error = %err,
                        "chat2 outbox: re-seal failed; batch retained");
                }
            }
        }
        out
    }

    fn retire(&self, batch_id: &str) {
        let receipt = lock(&self.outbox).remove(batch_id);
        if let Some(receipt) = receipt {
            match self.store.acknowledge_encrypted_batch(&receipt) {
                Ok(true) => {}
                Ok(false) => tracing::debug!(chat = %self.chat_id, batch_id,
                    "chat2 outbox: batch already retired"),
                Err(err) => tracing::warn!(chat = %self.chat_id, batch_id, error = %err,
                    "chat2 outbox: retire failed (will be replayed and deduped)"),
            }
        }
    }
}

/// Wire batch id of a sealed record: its immutable revision id, hex.
pub fn batch_id_of(revision_id: &[u8; 16]) -> String {
    revision_id.iter().map(|b| format!("{b:02x}")).collect()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ChatDocSink for EngineChatSink {
    fn apply_row(&self, bytes: &[u8], cursor: u64) -> ApplyOutcome {
        let Some(doc) = self.doc.upgrade() else {
            return ApplyOutcome::Applied;
        };
        let update = match self.open_update(bytes) {
            Ok(update) => update,
            Err(outcome) => {
                tracing::warn!(chat = %self.chat_id, ?outcome, cursor,
                    "chat2 sink: row not opened; cursor held");
                return outcome;
            }
        };
        match doc.doc().import(&update) {
            Ok(status) => {
                if status.pending.is_some() {
                    // Room sequence contiguity does not prove causal history
                    // is present. Snapshot export omits parked operations;
                    // advancing its cursor would lose them after restart.
                    tracing::warn!(chat = %self.chat_id, cursor,
                        "chat2 sink: row parked on missing deps; requesting repair");
                    return ApplyOutcome::PendingDependencies;
                }
            }
            Err(err) => {
                // Malformed (but, for encrypted rooms, AUTHENTICATED) update
                // bytes cost the row, never the doc — the same skip-not-fail
                // rule as transcript reads. The cursor still advances:
                // replaying a poison row forever is the wedge class.
                tracing::warn!(chat = %self.chat_id, error = %err,
                    "chat2 sink: row import failed; skipping row");
            }
        }
        match self.persist_with_cursor(cursor) {
            Ok(()) => ApplyOutcome::Applied,
            Err(()) => ApplyOutcome::StorageFailed,
        }
    }

    fn apply_checkpoint(&self, bytes: &[u8], cursor: u64) -> ApplyOutcome {
        let Some(doc) = self.doc.upgrade() else {
            return ApplyOutcome::StorageFailed;
        };
        let snapshot = match &self.codec {
            None => bytes.to_vec(),
            Some(codec) => match codec.open(
                ContentPurpose::Checkpoint,
                bytes,
                content::MAX_PLAINTEXT_BYTES,
            ) {
                Ok(snapshot) => snapshot,
                Err(outcome) => {
                    tracing::warn!(chat = %self.chat_id, ?outcome,
                        "chat2 sink: checkpoint not opened; cursor held");
                    return outcome;
                }
            },
        };
        match doc.doc().import(&snapshot) {
            Ok(status) if status.pending.is_some() => return ApplyOutcome::PendingDependencies,
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err, "chat2 sink: checkpoint import failed");
                return ApplyOutcome::StorageFailed;
            }
        }
        match self.persist_with_cursor(cursor) {
            Ok(()) => ApplyOutcome::Applied,
            Err(()) => ApplyOutcome::StorageFailed,
        }
    }

    fn contains_frontier(&self, frontier: &[u8]) -> bool {
        let Some(doc) = self.doc.upgrade() else {
            return true; // evicted: claim contained so the client idles, not refetches
        };
        // Encrypted rooms: an unopenable frontier is NOT contained (fetch),
        // never "already have it" (RFC 0001 §8).
        let opened;
        let frontier = match &self.codec {
            None => frontier,
            Some(codec) => match codec.open(ContentPurpose::Frontier, frontier, MAX_FRONTIER_BYTES)
            {
                Ok(bytes) => {
                    opened = bytes;
                    &opened
                }
                Err(outcome) => {
                    tracing::info!(chat = %self.chat_id, ?outcome,
                        "chat2 frontier not opened; fetching checkpoint");
                    return false;
                }
            },
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
        let _ = self.persist_with_cursor(cursor);
    }

    fn acknowledged(&self, batch_id: &str) {
        self.retire(batch_id);
    }

    fn rejected(&self, batch_id: &str) {
        // The ops stay in the local doc and reach peers via the next
        // checkpoint; the immutable ciphertext has no further use.
        self.retire(batch_id);
    }
}

/// `GET /chat2/{room}/checkpoint` with Range resume — the fetcher half of
/// the C1 client contract. Partial downloads resume at the byte offset where
/// the previous attempt died (the DO serves 206), which is the entire point
/// of checkpoint-over-HTTP on the 1.2 Mbps links this design targets.
pub struct EdgeCheckpointFetcher {
    http: reqwest::Client,
    edge: EdgeConfig,
    room_id: String,
}

impl EdgeCheckpointFetcher {
    pub fn new(http: reqwest::Client, edge: EdgeConfig, room_id: impl Into<String>) -> Self {
        Self {
            http,
            edge,
            room_id: room_id.into(),
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
            self.room_id
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
                // on change, restart the download from byte 0. (Encrypted
                // checkpoints additionally authenticate the whole object
                // before import, so a splice can never materialize.)
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
/// `/chat2/{room}/rows` with the same bearer auth the checkpoint fetcher uses.
pub struct EdgeChatTransport {
    http: reqwest::Client,
    edge: EdgeConfig,
    room_id: String,
    device_id: String,
}

impl EdgeChatTransport {
    pub fn new(
        http: reqwest::Client,
        edge: EdgeConfig,
        room_id: impl Into<String>,
        device_id: impl Into<String>,
    ) -> Self {
        Self {
            http,
            edge,
            room_id: room_id.into(),
            device_id: device_id.into(),
        }
    }

    fn rows_url(&self) -> String {
        format!(
            "{}/chat2/{}/rows",
            self.edge.url.trim_end_matches('/'),
            self.room_id
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

    #[test]
    fn encrypted_room_ids_stay_within_the_edge_id_grammar() {
        assert_eq!(encrypted_room_id("chat-1"), "chat-1-e1");
        let long = "x".repeat(130);
        let id = encrypted_room_id(&long);
        assert!(id.len() <= 128);
        assert!(id.starts_with("e1-"));
        assert_eq!(id, encrypted_room_id(&long));
        assert_ne!(id, encrypted_room_id(&"y".repeat(130)));
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
        assert_eq!(sink.apply_row(&row, 1), ApplyOutcome::PendingDependencies);
        let (_, cursor, _) = store.load_snapshot_with_cursor("chat").unwrap().unwrap();
        assert_eq!(cursor, 0, "a restart must retry the invisible update");

        sink.apply_checkpoint(&checkpoint, 0).unwrap();
        assert_eq!(sink.apply_row(&row, 1), ApplyOutcome::Applied);
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
