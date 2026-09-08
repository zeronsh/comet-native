//! Workspace registry — the client side of the row-table sidebar sync that
//! replaces the Loro workspace doc (docs/registry-sync.md).
//!
//! [`RegistryDoc`] is a local replica of the per-user registry room:
//! - `authoritative` rows — the server's truth, replaced wholesale by `state`/
//!   `rows` frames (the server merges; clients never re-merge server rows);
//! - `pending` op batches — local writes not yet acked, replayed over the
//!   authoritative rows for every read (optimistic overlay), re-pushed on
//!   reconnect (idempotent by strict-`>` clock compare);
//! - an HLC clock stamping every local write.
//!
//! The merge function [`apply_op`] mirrors `edge/src/registry-core.ts` 1:1 —
//! the shared test vectors live in both files; change them together. The
//! typed API mirrors the old `WorkspaceDoc` surface so `WorkspaceHost` is a
//! drop-in swap.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use zeron_proto::{Chat, ChatConfig, Device, Session, Space};

use crate::schema::DocError;
use crate::workspace::{DeletedSpace, WorkspaceState};

/// Row kinds — the four sidebar tables.
pub const KIND_DEVICES: &str = "devices";
pub const KIND_SPACES: &str = "spaces";
pub const KIND_CHATS: &str = "chats";
pub const KIND_SESSIONS: &str = "sessions";

/// Snapshot row id in the local `DocsStore` for the persisted registry state.
pub const REGISTRY_DOC_ID: &str = "registry1";

// ── HLC ─────────────────────────────────────────────────────────────────────

/// Encode an HLC string: `{ms:013}-{counter:06}-{device}`. Fixed-width zero
/// padding makes lexicographic order = (ms, counter, device) order, and the
/// device suffix makes the order total (two writers can never tie).
pub fn encode_hlc(ms: i64, counter: u32, device: &str) -> String {
    format!("{ms:013}-{counter:06}-{device}")
}

/// `a` strictly newer than `b` (`None` = never written, loses to any).
fn hlc_newer(a: &str, b: Option<&str>) -> bool {
    match b {
        None => true,
        Some(b) => a > b,
    }
}

/// Monotonic HLC source: never emits the same or an earlier clock twice, even
/// across a wall-clock regression or restart (state persists with the doc).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HlcClock {
    last_ms: i64,
    counter: u32,
}

impl HlcClock {
    fn next(&mut self, now_ms: i64, device: &str) -> String {
        if now_ms > self.last_ms {
            self.last_ms = now_ms;
            self.counter = 0;
        } else {
            self.counter += 1;
            if self.counter > 999_999 {
                self.last_ms += 1;
                self.counter = 0;
            }
        }
        encode_hlc(self.last_ms, self.counter, device)
    }
}

// ── rows and ops (wire-compatible with edge/src/registry-core.ts) ───────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryRow {
    pub kind: String,
    pub id: String,
    /// Server seq of the batch that last touched this row (0 locally).
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub deleted: bool,
    /// Tombstone clock — an upsert newer than this revives the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub del_hlc: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
    /// Per-field last-write clocks.
    #[serde(default)]
    pub clocks: BTreeMap<String, String>,
}

impl RegistryRow {
    fn tombstone(kind: &str, id: &str, hlc: String) -> Self {
        Self {
            kind: kind.to_string(),
            id: id.to_string(),
            seq: 0,
            deleted: true,
            del_hlc: Some(hlc),
            fields: BTreeMap::new(),
            clocks: BTreeMap::new(),
        }
    }

    /// The newest clock anywhere on the row (delete-vs-live comparison base).
    fn max_clock(&self) -> Option<&str> {
        let mut max = self.del_hlc.as_deref();
        for clock in self.clocks.values() {
            if max.is_none_or(|m| clock.as_str() > m) {
                max = Some(clock);
            }
        }
        max
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpKind {
    /// Creates, and revives tombstones when newer.
    Upsert,
    /// Never creates or revives ("never invent rows").
    Update,
    /// Tombstones when causally newer than the row.
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowOp {
    pub kind: String,
    pub id: String,
    pub op: OpKind,
    /// Field writes; `Value::Null` deletes the field (still a clocked write).
    /// Absent for deletes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<BTreeMap<String, Value>>,
    /// Clock for every write in `set` without an entry in `clocks`.
    pub hlc: String,
    /// Per-field clock overrides — re-seed pushes carry a row's ORIGINAL
    /// clocks so recovery never coarsens causality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clocks: Option<BTreeMap<String, String>>,
}

impl RowOp {
    fn clock_for<'a>(&'a self, field: &str) -> &'a str {
        self.clocks
            .as_ref()
            .and_then(|c| c.get(field))
            .map_or(self.hlc.as_str(), String::as_str)
    }
}

/// Apply one op to a row — the 1:1 mirror of `applyOp` in
/// `edge/src/registry-core.ts`. Returns the new row (`None` only for an
/// `update` on a missing row) and whether anything changed.
pub fn apply_op(row: Option<&RegistryRow>, op: &RowOp) -> (Option<RegistryRow>, bool) {
    if op.op == OpKind::Delete {
        return match row {
            // Tombstone-on-missing guards against a late create racing the delete.
            None => (
                Some(RegistryRow::tombstone(&op.kind, &op.id, op.hlc.clone())),
                true,
            ),
            Some(row) => {
                let beats = if row.deleted {
                    hlc_newer(&op.hlc, row.del_hlc.as_deref())
                } else {
                    hlc_newer(&op.hlc, row.max_clock())
                };
                if beats {
                    let mut gone = row.clone();
                    gone.deleted = true;
                    gone.del_hlc = Some(op.hlc.clone());
                    gone.fields.clear();
                    gone.clocks.clear();
                    (Some(gone), true)
                } else {
                    (Some(row.clone()), false)
                }
            }
        };
    }

    let mut base = match row {
        None => {
            if op.op == OpKind::Update {
                return (None, false);
            }
            RegistryRow {
                kind: op.kind.clone(),
                id: op.id.clone(),
                seq: 0,
                deleted: false,
                del_hlc: None,
                fields: BTreeMap::new(),
                clocks: BTreeMap::new(),
            }
        }
        Some(row) if row.deleted => {
            if op.op == OpKind::Update || !hlc_newer(&op.hlc, row.del_hlc.as_deref()) {
                return (Some(row.clone()), false);
            }
            // Revival: the tombstone loses wholesale; the upsert's fields are
            // the row.
            RegistryRow {
                kind: row.kind.clone(),
                id: row.id.clone(),
                seq: row.seq,
                deleted: false,
                del_hlc: row.del_hlc.clone(),
                fields: BTreeMap::new(),
                clocks: BTreeMap::new(),
            }
        }
        Some(row) => row.clone(),
    };

    let mut changed = row.is_none() || (row.is_some_and(|r| r.deleted) && !base.deleted);
    if let Some(set) = &op.set {
        for (key, value) in set {
            let clock = op.clock_for(key);
            if !hlc_newer(clock, base.clocks.get(key).map(String::as_str)) {
                continue;
            }
            if value.is_null() {
                base.fields.remove(key);
            } else {
                base.fields.insert(key.clone(), value.clone());
            }
            base.clocks.insert(key.clone(), clock.to_string());
            changed = true;
        }
    }
    if changed {
        base.deleted = false;
        (Some(base), true)
    } else {
        (Some(base), false)
    }
}

/// A row as a re-seed op (server-behind-client recovery): one upsert carrying
/// the row's ORIGINAL per-field clocks, or a delete for tombstones.
pub fn row_to_seed_op(row: &RegistryRow) -> RowOp {
    if row.deleted {
        return RowOp {
            kind: row.kind.clone(),
            id: row.id.clone(),
            op: OpKind::Delete,
            set: None,
            hlc: row
                .del_hlc
                .clone()
                .unwrap_or_else(|| encode_hlc(0, 0, "seed")),
            clocks: None,
        };
    }
    RowOp {
        kind: row.kind.clone(),
        id: row.id.clone(),
        op: OpKind::Upsert,
        set: Some(
            row.fields
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        hlc: row
            .max_clock()
            .map(str::to_string)
            .unwrap_or_else(|| encode_hlc(0, 0, "seed")),
        clocks: Some(row.clocks.clone()),
    }
}

// ── pending batches ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingBatch {
    pub batch: String,
    pub ops: Vec<RowOp>,
    /// True while the batch is in flight on the CURRENT connection (cleared
    /// on disconnect so reconnects re-push). Not persisted meaningfully — a
    /// restart implies a fresh connection.
    #[serde(default, skip_serializing)]
    pub in_flight: bool,
}

// ── the doc ─────────────────────────────────────────────────────────────────

/// What [`RegistryDoc::apply_state`] decided about a `state` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateOutcome {
    /// Delta applied over existing authoritative rows.
    Delta,
    /// Full state replaced local authoritative rows.
    Replaced,
    /// The server is BEHIND this client (wiped/reset storage): local rows were
    /// kept and a re-seed batch was enqueued. The caller must push pending.
    Reseeded,
}

/// Bump to force every client ONE full resync on its next boot (persisted
/// snapshots below this epoch zero their cursor on load). 1 = the
/// cursor-jump healing (ack/gap fixes below).
const CURRENT_RESYNC_EPOCH: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedState {
    v: u32,
    /// Cursor-integrity epoch: bumping [`CURRENT_RESYNC_EPOCH`] forces every
    /// client one full resync on its first load after upgrading (heals
    /// snapshots whose cursor jumped past unapplied rows). Additive —
    /// pre-epoch snapshots default to 0.
    #[serde(default)]
    resync_epoch: u32,
    device_id: String,
    server_seq: u64,
    gc_floor: u64,
    clock: HlcClock,
    rows: Vec<RegistryRow>,
    pending: Vec<PendingBatch>,
}

/// The local registry replica. Pure data — no I/O, no async; the transport
/// (`zeron_sync::RegistryClient`) and the engine host drive it under a lock.
pub struct RegistryDoc {
    device_id: String,
    /// kind → id → row (server truth).
    authoritative: HashMap<String, HashMap<String, RegistryRow>>,
    server_seq: u64,
    gc_floor: u64,
    clock: HlcClock,
    pending: Vec<PendingBatch>,
    /// Bumped on every mutation (local or applied) — the engine host converts
    /// this into watch-channel publishes and snapshot debounces.
    generation: u64,
}

impl RegistryDoc {
    pub fn new(device_id: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
            authoritative: HashMap::new(),
            server_seq: 0,
            gc_floor: 0,
            clock: HlcClock::default(),
            pending: Vec::new(),
            generation: 0,
        }
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Sync cursor: the last server seq this replica has fully applied.
    pub fn cursor(&self) -> u64 {
        self.server_seq
    }

    /// Monotonic local change counter (any mutation bumps it).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    // ── persistence ─────────────────────────────────────────────────────────

    // (see PersistedState::resync_epoch)

    pub fn to_bytes(&self) -> Result<Vec<u8>, DocError> {
        let rows = self
            .authoritative
            .values()
            .flat_map(|m| m.values().cloned())
            .collect();
        let state = PersistedState {
            v: 1,
            resync_epoch: CURRENT_RESYNC_EPOCH,
            device_id: self.device_id.clone(),
            server_seq: self.server_seq,
            gc_floor: self.gc_floor,
            clock: self.clock.clone(),
            rows,
            pending: self.pending.clone(),
        };
        Ok(serde_json::to_vec(&state)?)
    }

    pub fn from_bytes(bytes: &[u8], device_id: &str) -> Result<Self, DocError> {
        let state: PersistedState = serde_json::from_slice(bytes)?;
        if state.v != 1 {
            return Err(DocError::Schema(format!(
                "unknown registry snapshot version {}",
                state.v
            )));
        }
        let mut doc = Self::new(device_id);
        // One-shot healing resync: snapshots from before a cursor-integrity
        // fix may have a cursor that JUMPED past rows this replica never
        // applied (the ack/gap bugs above) — those rows are invisible and no
        // amount of ordinary syncing refetches them. Zeroing the cursor once
        // forces the next hello/pull to return full state; local rows are
        // kept (and local-only ones re-seed through apply_state's full path).
        if state.resync_epoch < CURRENT_RESYNC_EPOCH {
            doc.server_seq = 0;
        } else {
            doc.server_seq = state.server_seq;
        }
        doc.gc_floor = state.gc_floor;
        doc.clock = state.clock;
        doc.pending = state.pending;
        for row in state.rows {
            doc.authoritative
                .entry(row.kind.clone())
                .or_default()
                .insert(row.id.clone(), row);
        }
        Ok(doc)
    }

    // ── server frames ───────────────────────────────────────────────────────

    /// Apply a hello `state` frame. See [`StateOutcome`] for the three shapes.
    pub fn apply_state(
        &mut self,
        seq: u64,
        full: bool,
        gc_floor: u64,
        rows: Vec<RegistryRow>,
    ) -> StateOutcome {
        self.generation += 1;
        if !full {
            for row in rows {
                self.put_authoritative(row);
            }
            self.server_seq = seq;
            self.gc_floor = gc_floor;
            return StateOutcome::Delta;
        }
        if seq < self.server_seq {
            // The server lost state (reset/wipe). Local rows are the only
            // copy: keep them and re-seed the server with ORIGINAL clocks.
            let seed: Vec<RowOp> = self
                .authoritative
                .values()
                .flat_map(|m| m.values())
                .map(row_to_seed_op)
                .collect();
            for row in rows {
                self.put_authoritative(row);
            }
            self.server_seq = seq;
            self.gc_floor = gc_floor;
            if !seed.is_empty() {
                self.enqueue_ops(seed);
            }
            return StateOutcome::Reseeded;
        }
        // Full replace — but local-only rows the server never saw (e.g. it
        // GC-jumped our cursor while we held unseeded rows) re-seed too.
        let mut incoming: HashMap<String, HashMap<String, RegistryRow>> = HashMap::new();
        for row in rows {
            incoming
                .entry(row.kind.clone())
                .or_default()
                .insert(row.id.clone(), row);
        }
        let mut seed: Vec<RowOp> = Vec::new();
        for (kind, by_id) in &self.authoritative {
            for (id, row) in by_id {
                if !incoming.get(kind).is_some_and(|m| m.contains_key(id)) {
                    seed.push(row_to_seed_op(row));
                }
            }
        }
        self.authoritative = incoming;
        self.server_seq = seq;
        self.gc_floor = gc_floor;
        if seed.is_empty() {
            StateOutcome::Replaced
        } else {
            self.enqueue_ops(seed);
            StateOutcome::Reseeded
        }
    }

    /// Apply a `rows` broadcast (merged truth for the touched rows).
    /// Apply one broadcast batch. Returns `false` on a SEQ GAP — frames
    /// between the cursor and this batch were missed (a hibernating DO, a
    /// half-dead socket): the rows still apply (fresh data), but the cursor
    /// holds so the caller can resync (reconnect → hello/pull from the true
    /// cursor backfills the window). Advancing across a gap made the missed
    /// rows permanently invisible.
    #[must_use]
    pub fn apply_rows(&mut self, seq: u64, rows: Vec<RegistryRow>) -> bool {
        self.generation += 1;
        for row in rows {
            self.put_authoritative(row);
        }
        if seq > self.server_seq + 1 {
            return false;
        }
        if seq > self.server_seq {
            self.server_seq = seq;
        }
        true
    }

    /// Retire an acked batch; returns whether it existed.
    ///
    /// Deliberately does NOT advance the sync cursor: the ack's `seq` is OUR
    /// batch's position, and other devices' batches may sit between the
    /// cursor and it. Jumping the cursor skipped those forever — on the
    /// HTTPS transport the very next pull used the jumped cursor, so a
    /// device that pushed anything (a seen-marker was enough) while behind
    /// permanently missed every row its peers wrote since its last sync
    /// (field incident: new sessions never appeared in a laptop's sidebar
    /// while updates to already-known rows kept flowing). The cursor only
    /// advances with actual row payloads (state/pull/contiguous broadcasts);
    /// re-pulling our own batch is an idempotent LWW no-op.
    pub fn ack_batch(&mut self, batch: &str, _seq: u64) -> bool {
        let before = self.pending.len();
        self.pending.retain(|b| b.batch != batch);
        if self.pending.len() != before {
            self.generation += 1;
            true
        } else {
            false
        }
    }

    /// Batches to push: everything not already in flight on this connection.
    pub fn take_pushable(&mut self) -> Vec<PendingBatch> {
        let mut out = Vec::new();
        for batch in &mut self.pending {
            if !batch.in_flight {
                batch.in_flight = true;
                out.push(batch.clone());
            }
        }
        out
    }

    /// Connection dropped: everything unacked becomes pushable again.
    pub fn mark_disconnected(&mut self) {
        for batch in &mut self.pending {
            batch.in_flight = false;
        }
    }

    fn put_authoritative(&mut self, row: RegistryRow) {
        self.authoritative
            .entry(row.kind.clone())
            .or_default()
            .insert(row.id.clone(), row);
    }

    // ── local writes ────────────────────────────────────────────────────────

    fn now_ms() -> i64 {
        Utc::now().timestamp_millis()
    }

    fn next_hlc(&mut self) -> String {
        let device = self.device_id.clone();
        self.clock.next(Self::now_ms(), &device)
    }

    fn enqueue_ops(&mut self, mut ops: Vec<RowOp>) {
        if ops.is_empty() {
            return;
        }
        // The registry room rejects any batch over its op cap (500), and a
        // rejected batch is a PERMANENT wedge: error frames carry no batch
        // id, so the client can never retire it — it replays and fails on
        // every reconnect, and every write queued behind it (session-status
        // rows included) never propagates again. `delete_space` on a
        // ≥250-chat space minted exactly such a batch. Chunk here so no
        // writer can ever produce one; chunks apply in order, each
        // atomically (only cross-chunk atomicity is given up).
        const MAX_OPS_PER_BATCH: usize = 400;
        self.generation += 1;
        while !ops.is_empty() {
            let tail = if ops.len() > MAX_OPS_PER_BATCH {
                ops.split_off(MAX_OPS_PER_BATCH)
            } else {
                Vec::new()
            };
            // Batch ids only need device-lifetime uniqueness; the HLC of a
            // fresh tick provides exactly that without a rng dependency.
            let batch = format!("b-{}", self.next_hlc());
            self.pending.push(PendingBatch {
                batch,
                ops,
                in_flight: false,
            });
            ops = tail;
        }
    }

    fn write(&mut self, kind: &str, id: &str, op: OpKind, set: BTreeMap<String, Value>) {
        let hlc = self.next_hlc();
        self.enqueue_ops(vec![RowOp {
            kind: kind.to_string(),
            id: id.to_string(),
            op,
            set: Some(set),
            hlc,
            clocks: None,
        }]);
    }

    fn delete_row_ops(&mut self, keys: &[(&str, &str)]) {
        let hlc = self.next_hlc();
        let ops = keys
            .iter()
            .map(|(kind, id)| RowOp {
                kind: (*kind).to_string(),
                id: (*id).to_string(),
                op: OpKind::Delete,
                set: None,
                hlc: hlc.clone(),
                clocks: None,
            })
            .collect();
        self.enqueue_ops(ops);
    }

    // ── overlay reads ───────────────────────────────────────────────────────

    /// The row as this device should display it: authoritative + pending ops.
    fn overlay_row(&self, kind: &str, id: &str) -> Option<RegistryRow> {
        let mut row = self
            .authoritative
            .get(kind)
            .and_then(|m| m.get(id))
            .cloned();
        for batch in &self.pending {
            for op in &batch.ops {
                if op.kind == kind && op.id == id {
                    let (next, _) = apply_op(row.as_ref(), op);
                    if let Some(next) = next {
                        row = Some(next);
                    }
                }
            }
        }
        row.filter(|r| !r.deleted)
    }

    /// All live rows of `kind`, overlay applied.
    fn overlay_rows(&self, kind: &str) -> Vec<RegistryRow> {
        let mut ids: Vec<String> = self
            .authoritative
            .get(kind)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        for batch in &self.pending {
            for op in &batch.ops {
                if op.kind == kind && !ids.iter().any(|id| id == &op.id) {
                    ids.push(op.id.clone());
                }
            }
        }
        ids.iter()
            .filter_map(|id| self.overlay_row(kind, id))
            .collect()
    }

    fn read_kind<T: serde::de::DeserializeOwned>(&self, kind: &str) -> Vec<T> {
        let mut out = Vec::new();
        for row in self.overlay_rows(kind) {
            let value = Value::Object(row.fields.clone().into_iter().collect());
            match serde_json::from_value::<T>(value) {
                Ok(parsed) => out.push(parsed),
                Err(err) => {
                    tracing::warn!(kind, row = %row.id, error = %err, "skipping malformed registry row");
                }
            }
        }
        out
    }

    fn row_exists(&self, kind: &str, id: &str) -> bool {
        self.overlay_row(kind, id).is_some()
    }

    // ── typed API (the WorkspaceDoc surface) ────────────────────────────────

    /// Upsert a full device row (writer discipline: callers pass their OWN device).
    pub fn upsert_device(&mut self, device: &Device) -> Result<(), DocError> {
        let set = fields([
            ("id", json!(device.id)),
            ("name", json!(device.name)),
            ("platform", json!(device.platform)),
            ("lastSeenAt", opt_ms(device.last_seen_at)),
            ("createdAt", opt_ms(device.created_at)),
            ("version", opt_str(device.version.as_deref())),
        ]);
        self.write(KIND_DEVICES, &device.id.clone(), OpKind::Upsert, set);
        Ok(())
    }

    /// LWW rename (settings UI; any device may write). `false` when no such row.
    pub fn rename_device(&mut self, device_id: &str, name: &str) -> Result<bool, DocError> {
        if !self.row_exists(KIND_DEVICES, device_id) {
            return Ok(false);
        }
        self.write(
            KIND_DEVICES,
            device_id,
            OpKind::Update,
            fields([("name", json!(name))]),
        );
        Ok(true)
    }

    /// Stamp `lastSeenAt` on an existing device row (boot/shutdown only —
    /// periodic liveness rides presence frames, never rows).
    pub fn set_device_last_seen(
        &mut self,
        device_id: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_DEVICES, device_id) {
            return Ok(false);
        }
        self.write(
            KIND_DEVICES,
            device_id,
            OpKind::Update,
            fields([("lastSeenAt", json!(at.timestamp_millis()))]),
        );
        Ok(true)
    }

    pub fn read_devices(&self) -> Result<Vec<Device>, DocError> {
        let mut devices: Vec<Device> = self
            .read_kind::<crate::workspace::RawDevice>(KIND_DEVICES)
            .into_iter()
            .map(Device::from)
            .collect();
        devices.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(devices)
    }

    // ── spaces ──────────────────────────────────────────────────────────────

    pub fn upsert_space(&mut self, space: &Space) -> Result<(), DocError> {
        let set = fields([
            ("id", json!(space.id)),
            ("deviceId", json!(space.device_id)),
            ("path", json!(space.path)),
            ("name", opt_str(space.name.as_deref())),
            ("gitDetected", json!(space.git_detected)),
            ("gitCheckedAt", opt_ms(space.git_checked_at)),
            ("checkoutId", opt_str(space.checkout_id.as_deref())),
            ("createdAt", json!(space.created_at.timestamp_millis())),
        ]);
        self.write(KIND_SPACES, &space.id.clone(), OpKind::Upsert, set);
        Ok(())
    }

    pub fn space(&self, space_id: &str) -> Result<Option<Space>, DocError> {
        Ok(self
            .overlay_row(KIND_SPACES, space_id)
            .and_then(|row| row_to::<crate::workspace::RawSpace>(&row))
            .map(Space::from))
    }

    pub fn read_spaces(&self) -> Result<Vec<Space>, DocError> {
        let mut spaces: Vec<Space> = self
            .read_kind::<crate::workspace::RawSpace>(KIND_SPACES)
            .into_iter()
            .map(Space::from)
            .collect();
        spaces.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(spaces)
    }

    /// LWW display-name set from any device; `None` clears back to the derived
    /// name (basename of path). `false` when no such row.
    pub fn rename_space(&mut self, space_id: &str, name: Option<&str>) -> Result<bool, DocError> {
        if !self.row_exists(KIND_SPACES, space_id) {
            return Ok(false);
        }
        self.write(
            KIND_SPACES,
            space_id,
            OpKind::Update,
            fields([("name", opt_str(name))]),
        );
        Ok(true)
    }

    /// Owner-stamped git presence for the space folder (ownership is asserted
    /// by the engine layer, this is mechanism only).
    pub fn set_space_git(
        &mut self,
        space_id: &str,
        detected: bool,
        checkout_id: Option<&str>,
        checked_at: DateTime<Utc>,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_SPACES, space_id) {
            return Ok(false);
        }
        self.write(
            KIND_SPACES,
            space_id,
            OpKind::Update,
            fields([
                ("gitDetected", json!(detected)),
                ("checkoutId", opt_str(checkout_id)),
                ("gitCheckedAt", json!(checked_at.timestamp_millis())),
            ]),
        );
        Ok(true)
    }

    /// Hard-delete a space and cascade to its chats: one batch tombstones the
    /// space row and every chat/session row whose `spaceId` matches — the
    /// server applies the batch atomically. Returns the removed chat ids so
    /// the engine can drop local state.
    pub fn delete_space(&mut self, space_id: &str) -> Result<DeletedSpace, DocError> {
        let existed = self.row_exists(KIND_SPACES, space_id);
        let chat_ids: Vec<String> = self
            .read_chats()?
            .into_iter()
            .filter(|c| c.space_id.as_deref() == Some(space_id))
            .map(|c| c.id)
            .collect();
        let mut keys: Vec<(&str, &str)> = Vec::with_capacity(chat_ids.len() * 2 + 1);
        for chat_id in &chat_ids {
            keys.push((KIND_CHATS, chat_id));
            keys.push((KIND_SESSIONS, chat_id));
        }
        keys.push((KIND_SPACES, space_id));
        self.delete_row_ops(&keys);
        Ok(DeletedSpace { existed, chat_ids })
    }

    // ── chats ───────────────────────────────────────────────────────────────

    pub fn upsert_chat(&mut self, chat: &Chat) -> Result<(), DocError> {
        let config = match &chat.config {
            Some(config) => serde_json::to_value(config)?,
            None => Value::Null,
        };
        let set = fields([
            ("id", json!(chat.id)),
            ("deviceId", json!(chat.device_id)),
            ("title", opt_str(chat.title.as_deref())),
            ("archived", json!(chat.archived)),
            ("cwd", opt_str(chat.cwd.as_deref())),
            ("branch", opt_str(chat.branch.as_deref())),
            ("checkoutId", opt_str(chat.checkout_id.as_deref())),
            (
                "sourceContext",
                chat.source_context
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()?
                    .unwrap_or(Value::Null),
            ),
            ("config", config),
            (
                "lastMessagePreview",
                opt_str(chat.last_message_preview.as_deref()),
            ),
            ("lastMessageAt", opt_ms(chat.last_message_at)),
            ("createdAt", json!(chat.created_at.timestamp_millis())),
            (
                "harnessSessionId",
                opt_str(chat.harness_session_id.as_deref()),
            ),
            (
                "harnessSessionCwd",
                opt_str(chat.harness_session_cwd.as_deref()),
            ),
            ("spaceId", opt_str(chat.space_id.as_deref())),
            ("lastSeenAt", opt_ms(chat.last_seen_at)),
            (
                "roomGen",
                chat.room_gen.map(|g| json!(g)).unwrap_or(Value::Null),
            ),
        ]);
        self.write(KIND_CHATS, &chat.id.clone(), OpKind::Upsert, set);
        Ok(())
    }

    /// Claim-on-first-command row: ONLY the fields the claim actually knows
    /// (identity, cwd, space). Absent fields carry no clocked write, so the
    /// real `createChat` upsert — racing behind on the registry channel with
    /// older clocks — still lands its `config`/`title` under per-field LWW
    /// instead of losing them to `Null` deletes.
    pub fn claim_chat(
        &mut self,
        chat_id: &str,
        cwd: Option<&str>,
        space_id: Option<&str>,
        created_at: DateTime<Utc>,
    ) {
        let mut set = fields([
            ("id", json!(chat_id)),
            ("deviceId", json!(self.device_id())),
            ("createdAt", json!(created_at.timestamp_millis())),
        ]);
        if let Some(cwd) = cwd {
            set.insert("cwd".into(), json!(cwd));
        }
        if let Some(space_id) = space_id {
            set.insert("spaceId".into(), json!(space_id));
        }
        self.write(KIND_CHATS, chat_id, OpKind::Upsert, set);
    }

    /// Synced seen marker (LWW) with a monotonic guard: no write when the
    /// stored stamp is already >= `at`.
    pub fn set_chat_seen(&mut self, chat_id: &str, at: DateTime<Utc>) -> Result<bool, DocError> {
        let Some(row) = self.overlay_row(KIND_CHATS, chat_id) else {
            return Ok(false);
        };
        let current = row.fields.get("lastSeenAt").and_then(Value::as_i64);
        if current.is_some_and(|ms| ms >= at.timestamp_millis()) {
            return Ok(true);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("lastSeenAt", json!(at.timestamp_millis()))]),
        );
        Ok(true)
    }

    pub fn chat(&self, chat_id: &str) -> Result<Option<Chat>, DocError> {
        Ok(self
            .overlay_row(KIND_CHATS, chat_id)
            .and_then(|row| row_to::<crate::workspace::RawChat>(&row))
            .map(Chat::from))
    }

    pub fn read_chats(&self) -> Result<Vec<Chat>, DocError> {
        let mut chats: Vec<Chat> = self
            .read_kind::<crate::workspace::RawChat>(KIND_CHATS)
            .into_iter()
            .map(Chat::from)
            .collect();
        chats.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(chats)
    }

    pub fn rename_chat(&mut self, chat_id: &str, title: &str) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("title", json!(title))]),
        );
        Ok(true)
    }

    pub fn set_chat_archived(&mut self, chat_id: &str, archived: bool) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("archived", json!(archived))]),
        );
        Ok(true)
    }

    /// Flip the chat's sync room generation (docs/chat2-sync.md M2). The
    /// host calls this in the same breath as seeding the chat2 checkpoint;
    /// LWW per-field like every registry write, so the flip is per-chat and
    /// instantly revertible by writing 1 back.
    pub fn set_chat_room_gen(&mut self, chat_id: &str, room_gen: u32) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("roomGen", json!(room_gen))]),
        );
        Ok(true)
    }

    pub fn set_chat_branch(&mut self, chat_id: &str, branch: &str) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("branch", json!(branch))]),
        );
        Ok(true)
    }

    pub fn set_chat_source_context(
        &mut self,
        chat_id: &str,
        context: &zeron_proto::ConversationSourceContext,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([
                ("sourceContext", serde_json::to_value(context)?),
                ("branch", json!(context.branch)),
                ("checkoutId", json!(context.checkout_id)),
            ]),
        );
        Ok(true)
    }

    /// Retarget the chat onto another folder — the mid-session "switch to an
    /// existing worktree" move. Harness resume is cwd-scoped, so the next run
    /// in the new folder starts a fresh harness conversation by design.
    pub fn set_chat_cwd(&mut self, chat_id: &str, cwd: &str) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("cwd", json!(cwd))]),
        );
        Ok(true)
    }

    pub fn set_chat_checkout(
        &mut self,
        chat_id: &str,
        checkout_id: &str,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("checkoutId", json!(checkout_id))]),
        );
        Ok(true)
    }

    pub fn set_chat_config(
        &mut self,
        chat_id: &str,
        config: &ChatConfig,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        let value = serde_json::to_value(config)?;
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([("config", value)]),
        );
        Ok(true)
    }

    /// Host-side resume continuity. An empty `session_id` is the explicit
    /// "do not resume" tombstone written after a harness rejects a resume.
    pub fn set_chat_harness_session(
        &mut self,
        chat_id: &str,
        session_id: &str,
        cwd: &str,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([
                ("harnessSessionId", json!(session_id)),
                ("harnessSessionCwd", json!(cwd)),
            ]),
        );
        Ok(true)
    }

    /// Host-side sidebar freshness: preview + timestamp of the latest message.
    pub fn set_chat_last_message(
        &mut self,
        chat_id: &str,
        preview: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, DocError> {
        if !self.row_exists(KIND_CHATS, chat_id) {
            return Ok(false);
        }
        self.write(
            KIND_CHATS,
            chat_id,
            OpKind::Update,
            fields([
                ("lastMessagePreview", json!(preview)),
                ("lastMessageAt", json!(at.timestamp_millis())),
            ]),
        );
        Ok(true)
    }

    /// Tombstone: delete the chat row (and its session-status row). The
    /// per-chat session doc remains — this removes the index entry only.
    pub fn delete_chat(&mut self, chat_id: &str) -> Result<bool, DocError> {
        let existed = self.row_exists(KIND_CHATS, chat_id);
        self.delete_row_ops(&[(KIND_CHATS, chat_id), (KIND_SESSIONS, chat_id)]);
        Ok(existed)
    }

    // ── sessions ────────────────────────────────────────────────────────────

    /// Upsert a session-status row (writer discipline: each device writes only
    /// its own runs' rows). Staleness is checked client-side via `updatedAt`.
    pub fn upsert_session(&mut self, session: &Session) -> Result<(), DocError> {
        let set = fields([
            ("chatId", json!(session.chat_id)),
            ("deviceId", json!(session.device_id)),
            ("status", serde_json::to_value(session.status)?),
            ("startedAt", opt_ms(session.started_at)),
            ("updatedAt", json!(session.updated_at.timestamp_millis())),
        ]);
        self.write(KIND_SESSIONS, &session.chat_id.clone(), OpKind::Upsert, set);
        Ok(())
    }

    pub fn read_sessions(&self) -> Result<Vec<Session>, DocError> {
        let mut sessions: Vec<Session> = self
            .read_kind::<crate::workspace::RawSession>(KIND_SESSIONS)
            .into_iter()
            .map(Session::from)
            .collect();
        sessions.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
        Ok(sessions)
    }

    // ── whole-doc read ──────────────────────────────────────────────────────

    pub fn read_all(&self) -> Result<WorkspaceState, DocError> {
        Ok(WorkspaceState {
            devices: self.read_devices()?,
            spaces: self.read_spaces()?,
            chats: self.read_chats()?,
            sessions: self.read_sessions()?,
        })
    }

    // ── migration ───────────────────────────────────────────────────────────

    /// Seed from the legacy Loro workspace doc's materialized state (first
    /// boot after the update). Every row becomes a pending upsert whose HLC
    /// derives from the row's own newest timestamp — historical, so any
    /// genuinely newer live write beats the migrated value; identical across
    /// devices, so N devices seeding the same converged doc is idempotent
    /// (equal values, deterministic device tie-break).
    pub fn seed_from_workspace(&mut self, state: &WorkspaceState) -> Result<usize, DocError> {
        let mut ops: Vec<RowOp> = Vec::new();
        let mut seed = |kind: &str, id: &str, ms: i64, set: BTreeMap<String, Value>| {
            ops.push(RowOp {
                kind: kind.to_string(),
                id: id.to_string(),
                op: OpKind::Upsert,
                set: Some(set),
                hlc: encode_hlc(ms.max(1), 0, "migration"),
                clocks: None,
            });
        };
        for device in &state.devices {
            let ms = newest(&[device.last_seen_at, device.created_at]);
            seed(
                KIND_DEVICES,
                &device.id,
                ms,
                fields([
                    ("id", json!(device.id)),
                    ("name", json!(device.name)),
                    ("platform", json!(device.platform)),
                    ("lastSeenAt", opt_ms(device.last_seen_at)),
                    ("createdAt", opt_ms(device.created_at)),
                    ("version", opt_str(device.version.as_deref())),
                ]),
            );
        }
        for space in &state.spaces {
            let ms = newest(&[space.git_checked_at, Some(space.created_at)]);
            seed(
                KIND_SPACES,
                &space.id,
                ms,
                fields([
                    ("id", json!(space.id)),
                    ("deviceId", json!(space.device_id)),
                    ("path", json!(space.path)),
                    ("name", opt_str(space.name.as_deref())),
                    ("gitDetected", json!(space.git_detected)),
                    ("gitCheckedAt", opt_ms(space.git_checked_at)),
                    ("checkoutId", opt_str(space.checkout_id.as_deref())),
                    ("createdAt", json!(space.created_at.timestamp_millis())),
                ]),
            );
        }
        for chat in &state.chats {
            let ms = newest(&[
                chat.last_message_at,
                chat.last_seen_at,
                Some(chat.created_at),
            ]);
            let config = match &chat.config {
                Some(config) => serde_json::to_value(config)?,
                None => Value::Null,
            };
            seed(
                KIND_CHATS,
                &chat.id,
                ms,
                fields([
                    ("id", json!(chat.id)),
                    ("deviceId", json!(chat.device_id)),
                    ("title", opt_str(chat.title.as_deref())),
                    ("archived", json!(chat.archived)),
                    ("cwd", opt_str(chat.cwd.as_deref())),
                    ("branch", opt_str(chat.branch.as_deref())),
                    ("checkoutId", opt_str(chat.checkout_id.as_deref())),
                    ("config", config),
                    (
                        "lastMessagePreview",
                        opt_str(chat.last_message_preview.as_deref()),
                    ),
                    ("lastMessageAt", opt_ms(chat.last_message_at)),
                    ("createdAt", json!(chat.created_at.timestamp_millis())),
                    (
                        "harnessSessionId",
                        opt_str(chat.harness_session_id.as_deref()),
                    ),
                    (
                        "harnessSessionCwd",
                        opt_str(chat.harness_session_cwd.as_deref()),
                    ),
                    ("spaceId", opt_str(chat.space_id.as_deref())),
                    ("lastSeenAt", opt_ms(chat.last_seen_at)),
                ]),
            );
        }
        for session in &state.sessions {
            let ms = newest(&[Some(session.updated_at), session.started_at]);
            seed(
                KIND_SESSIONS,
                &session.chat_id,
                ms,
                fields([
                    ("chatId", json!(session.chat_id)),
                    ("deviceId", json!(session.device_id)),
                    ("status", serde_json::to_value(session.status)?),
                    ("startedAt", opt_ms(session.started_at)),
                    ("updatedAt", json!(session.updated_at.timestamp_millis())),
                ]),
            );
        }
        let count = ops.len();
        // Chunk so a huge legacy workspace never exceeds the server's batch cap.
        for chunk in ops.chunks(400) {
            self.enqueue_ops(chunk.to_vec());
        }
        Ok(count)
    }
}

// ── field helpers ───────────────────────────────────────────────────────────

fn fields<const N: usize>(entries: [(&str, Value); N]) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn opt_str(value: Option<&str>) -> Value {
    match value {
        Some(v) => json!(v),
        None => Value::Null,
    }
}

fn opt_ms(value: Option<DateTime<Utc>>) -> Value {
    match value {
        Some(at) => json!(at.timestamp_millis()),
        None => Value::Null,
    }
}

fn newest(candidates: &[Option<DateTime<Utc>>]) -> i64 {
    candidates
        .iter()
        .flatten()
        .map(|at| at.timestamp_millis())
        .max()
        .unwrap_or(1)
}

fn row_to<T: serde::de::DeserializeOwned>(row: &RegistryRow) -> Option<T> {
    let value = Value::Object(row.fields.clone().into_iter().collect());
    match serde_json::from_value::<T>(value) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            tracing::warn!(kind = %row.kind, row = %row.id, error = %err, "skipping malformed registry row");
            None
        }
    }
}

// SessionStatus needs to serialize to the same strings the loro doc used
// ("idle"/"working"/…) — zeron_proto's serde derives already use camelCase;
// the compile-time check lives in the tests below.

#[cfg(test)]
mod tests;
