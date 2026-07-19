/**
 * SessionRoom — one Durable Object per doc room, speaking loro-protocol over
 * hibernatable WebSockets (design §2, §3.1). Two doc kinds share this class:
 * chat session docs (room name = chatId, claim-on-first-join ownership) and
 * workspace docs (room name = `ws/{orgId}`, org-membership authz enforced by
 * the Worker — the DO sees the ROOM_KIND_HEADER stamp and skips ownership).
 *
 * Persistence model:
 * - `updates` — append-only incoming update log, buffered in memory during
 *   active streams and flushed every ~DO_FLUSH_MS (a crash losing buffered
 *   ops is healed by normal CRDT resync from the host on reconnect).
 * - `snapshot` blob — the doc's current snapshot. Two-level compaction:
 *   LOG FOLD (whenever the update log passes COMPACT_LOG_BYTES): re-export a
 *   full snapshot and clear the log — loses nothing. HISTORY TRIM (daily
 *   alarm): once a recorded frontier checkpoint is older than RETAIN_DAYS,
 *   re-export a *shallow* snapshot at that frontier — trimmed op history is
 *   discarded permanently, state is fully preserved (§3.1).
 * - `tail` blob — materialized last-N-messages JSON, recomputed lazily on
 *   GET /tail when dirty (§5 L2).
 * - `diff` blob — latest-only working-tree diff sidecar, overwritten on each
 *   host publish (§6.1).
 * - Ephemeral presence (%EPH room) is memory-only by construction.
 *
 * Hibernation discipline: no wall-clock JS timers except the flush debounce
 * (which only exists while traffic keeps the DO awake anyway); scheduled work
 * (checkpoints, history trim, R2 backup §3.3) rides the durable alarm.
 */
import { LoroDoc, EphemeralStore, VersionVector } from "loro-crdt";
import {
  CrdtType,
  JoinErrorCode,
  MAX_MESSAGE_SIZE,
  MessageType,
  UpdateStatusCode,
  bytesToHex,
  decode,
  encode,
  type DocUpdate,
  type DocUpdateFragmentHeader,
  type JoinRequest,
  type ProtocolMessage
} from "loro-protocol";
import { COMPACT_LOG_BYTES, DO_FLUSH_MS, RETAIN_DAYS, materializeTail } from "./session-doc";
import { createBlobStore, getJsonBlob, putJsonBlob, type BlobStore } from "./blobs";
import { AUTH_USER_HEADER, ROOM_KIND_HEADER, type Env } from "./env";

const DAY_MS = 24 * 60 * 60 * 1000;
const RETAIN_MS = RETAIN_DAYS * DAY_MS;
/** Payload bytes per outbound fragment (leaves room for the envelope). */
const FRAGMENT_BYTES = 200_000;
/** Keep a rolling ~5 weeks of daily frontier checkpoints. */
const MAX_CHECKPOINTS = 36;

interface SocketState {
  userId: string;
  /** Joined sub-rooms by crdt magic ("%LOR", "%EPH"). */
  rooms: string[];
  /** True for sockets on a workspace-doc room — org membership was enforced
   * by the Worker, so the per-chat ownership discipline does not apply. */
  workspace?: boolean;
}

interface FragmentBatch {
  parts: Uint8Array[];
  received: number;
  totalSize: number;
  header: DocUpdateFragmentHeader;
}

interface FrontierCheckpoint {
  at: number;
  frontiers: { peer: string; counter: number }[];
}

export class SessionRoom implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;
  private readonly blobs: BlobStore;
  /** Lazily materialized doc — the log is authoritative; this is a cache. */
  private doc: LoroDoc | undefined;
  private eph: EphemeralStore | undefined;
  private pending: Uint8Array[] = [];
  private pendingBytes = 0;
  private flushTimer: ReturnType<typeof setTimeout> | undefined;
  /** In-memory fragment reassembly. Lost on hibernation → the sender gets a
   * FragmentTimeout ack for the unknown batch and resends — self-healing. */
  private readonly fragments = new Map<WebSocket, Map<string, FragmentBatch>>();

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS updates (seq INTEGER PRIMARY KEY AUTOINCREMENT, bytes BLOB NOT NULL, received_at INTEGER NOT NULL)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
    );
    this.blobs = createBlobStore(ctx.storage.sql);
    // Protocol-designed hibernation keepalive: ping → pong without waking us.
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  // ── meta helpers ──────────────────────────────────────────────────────────

  private getMeta(key: string): string | undefined {
    const rows = [...this.ctx.storage.sql.exec("SELECT value FROM meta WHERE key = ?", key)];
    return rows[0]?.value as string | undefined;
  }

  private setMeta(key: string, value: string): void {
    this.ctx.storage.sql.exec(
      "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      key,
      value
    );
  }

  // ── HTTP surface (only reachable through the authed Worker) ──────────────

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return new Response("unauthenticated", { status: 401 });
    // Workspace rooms: the Worker already checked org membership; every
    // member may read/write, so the owner gates below are bypassed.
    const workspace = request.headers.get(ROOM_KIND_HEADER) === "workspace";

    if (url.pathname === "/ws") {
      const chatId = url.searchParams.get("chatId") ?? "";
      if (chatId && !this.getMeta("chatId")) this.setMeta("chatId", chatId);
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1]);
      const state: SocketState = { userId, rooms: [], ...(workspace ? { workspace } : {}) };
      pair[1].serializeAttachment(state);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    const owner = this.getMeta("owner");
    if (url.pathname === "/stats" && request.method === "GET") {
      // Observability: what this room holds and who's on it. Owner-gated like
      // every other read (org-membership-gated for workspace rooms).
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      this.flush();
      const updateRows = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]
        ?.n as number;
      const snapshot = this.blobs.get("snapshot");
      return json({
        chatId: this.getMeta("chatId") ?? null,
        connectedSockets: this.ctx.getWebSockets().length,
        updateRows,
        updateLogBytes: Number(this.getMeta("updateBytes") ?? "0"),
        snapshotBytes: snapshot?.length ?? 0,
        tailCached: this.getMeta("tailDirty") !== "1" && this.blobs.get("tail") !== undefined,
        diffPublished: this.blobs.get("diff") !== undefined,
        checkpoints: (JSON.parse(this.getMeta("checkpoints") ?? "[]") as unknown[]).length,
        lastTrimAt: this.getMeta("lastTrimAt") ?? null,
        backupDirty: this.getMeta("backupDirty") === "1"
      });
    }
    if (url.pathname === "/tail" && request.method === "GET") {
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      return json(this.currentTail());
    }
    if (url.pathname === "/diff" && request.method === "GET") {
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      const diff = getJsonBlob<unknown>(this.blobs, "diff");
      return diff === undefined ? json({ error: "not_found" }, 404) : json(diff);
    }
    if (url.pathname === "/diff" && request.method === "POST") {
      // The host may publish before any room join has claimed the doc.
      if (!workspace) {
        if (!owner) this.setMeta("owner", userId);
        else if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      putJsonBlob(this.blobs, "diff", await request.json());
      return json({ ok: true });
    }
    if (url.pathname === "/snapshot" && request.method === "GET") {
      // Repair/inspection read: the doc's full current snapshot bytes.
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      this.flush();
      const doc = this.ensureDoc();
      const bytes = doc.export({ mode: "snapshot" });
      return new Response(bytes as unknown as BodyInit, {
        headers: { "content-type": "application/octet-stream" }
      });
    }
    if (url.pathname === "/append" && request.method === "POST") {
      // MERGE-safe repair write: import a Loro update (never replaces the
      // doc). Same durability bookkeeping as a WS DocUpdate.
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      const body = new Uint8Array(await request.arrayBuffer());
      const doc = this.ensureDoc();
      try {
        if (body.length > 0) doc.import(body);
      } catch {
        return json({ error: "invalid_update" }, 400);
      }
      this.recordLoroUpdates([body]);
      // Converge live peers: relay the update to connected %LOR sockets.
      const roomId = this.getMeta("chatId") ?? "";
      for (const ws of this.ctx.getWebSockets()) {
        const state = ws.deserializeAttachment() as SocketState | null;
        if (!state?.rooms.includes(CrdtType.Loro)) continue;
        this.sendUpdates(ws, CrdtType.Loro, roomId, [body]);
      }
      return json({ ok: true });
    }
    return new Response("not found", { status: 404 });
  }

  // ── WebSocket protocol ────────────────────────────────────────────────────

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    if (typeof message === "string") return; // ping/pong handled by auto-response
    let decoded: ProtocolMessage;
    try {
      decoded = decode(new Uint8Array(message));
    } catch {
      ws.close(1002, "Protocol error");
      return;
    }
    const state = ws.deserializeAttachment() as SocketState;
    switch (decoded.type) {
      case MessageType.JoinRequest:
        this.handleJoin(ws, state, decoded);
        break;
      case MessageType.DocUpdate:
        this.handleDocUpdate(ws, state, decoded);
        break;
      case MessageType.DocUpdateFragmentHeader:
        this.handleFragmentHeader(ws, state, decoded);
        break;
      case MessageType.DocUpdateFragment:
        this.handleFragment(ws, state, decoded);
        break;
      case MessageType.Leave:
        state.rooms = state.rooms.filter((r) => r !== decoded.crdt);
        ws.serializeAttachment(state);
        break;
      case MessageType.Ack:
      case MessageType.RoomError:
        break;
      default:
        ws.close(1002, "Unsupported message");
    }
  }

  webSocketClose(ws: WebSocket): void {
    this.fragments.delete(ws);
    this.flush();
  }

  webSocketError(ws: WebSocket): void {
    this.fragments.delete(ws);
    this.flush();
  }

  private handleJoin(ws: WebSocket, state: SocketState, message: JoinRequest): void {
    if (!state.workspace) {
      // Chat rooms: claim-on-first-join ownership, then owner-only forever.
      const owner = this.getMeta("owner");
      if (!owner) this.setMeta("owner", state.userId);
      else if (owner !== state.userId) {
        this.send(ws, {
          type: MessageType.JoinError,
          crdt: message.crdt,
          roomId: message.roomId,
          code: JoinErrorCode.AuthFailed,
          message: "not the room owner"
        });
        return;
      }
    }
    if (!this.getMeta("chatId") && message.roomId) this.setMeta("chatId", message.roomId);

    if (message.crdt === CrdtType.Loro) {
      const doc = this.ensureDoc();
      if (!state.rooms.includes(message.crdt)) state.rooms.push(message.crdt);
      ws.serializeAttachment(state);
      this.send(ws, {
        type: MessageType.JoinResponseOk,
        crdt: message.crdt,
        roomId: message.roomId,
        permission: "write",
        version: doc.version().encode()
      });
      let backfill: Uint8Array;
      if (message.version.length > 0) {
        try {
          backfill = doc.export({ mode: "update", from: VersionVector.decode(message.version) });
        } catch {
          // Unknown/garbled client version — fall back to a full snapshot.
          backfill = doc.export({ mode: "snapshot" });
        }
      } else {
        backfill = doc.export({ mode: "snapshot" });
      }
      if (backfill.length > 0) {
        this.sendUpdates(ws, message.crdt, message.roomId, [backfill]);
      }
      return;
    }

    if (message.crdt === CrdtType.LoroEphemeralStore) {
      const eph = this.ensureEph();
      if (!state.rooms.includes(message.crdt)) state.rooms.push(message.crdt);
      ws.serializeAttachment(state);
      this.send(ws, {
        type: MessageType.JoinResponseOk,
        crdt: message.crdt,
        roomId: message.roomId,
        permission: "write",
        version: new Uint8Array()
      });
      const all = eph.encodeAll();
      if (all.length > 0) this.sendUpdates(ws, message.crdt, message.roomId, [all]);
      return;
    }

    this.send(ws, {
      type: MessageType.JoinError,
      crdt: message.crdt,
      roomId: message.roomId,
      code: JoinErrorCode.Unknown,
      message: "unsupported crdt"
    });
  }

  private handleDocUpdate(ws: WebSocket, state: SocketState, message: DocUpdate): void {
    if (message.updates.some((u) => u.length > MAX_MESSAGE_SIZE)) {
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge);
      return;
    }
    if (!state.rooms.includes(message.crdt)) {
      this.ack(ws, message, UpdateStatusCode.PermissionDenied);
      return;
    }
    this.applyUpdates(ws, state, message.crdt, message.roomId, message.batchId, message.updates);
  }

  /** Shared apply path for whole and reassembled updates. */
  private applyUpdates(
    ws: WebSocket,
    _state: SocketState,
    crdt: CrdtType,
    roomId: string,
    batchId: `0x${string}`,
    updates: Uint8Array[]
  ): void {
    if (crdt === CrdtType.Loro) {
      const doc = this.ensureDoc();
      try {
        for (const update of updates) if (update.length > 0) doc.import(update);
      } catch {
        // Includes imports concurrent to a shallow-snapshot start (§3.1 stale
        // peer) — the client resyncs fresh and re-submits at the app layer.
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      this.recordLoroUpdates(updates);
      this.ack(ws, { crdt, roomId }, UpdateStatusCode.Ok, batchId);
      this.broadcast(ws, crdt, { type: MessageType.DocUpdate, crdt, roomId, updates, batchId });
      return;
    }
    if (crdt === CrdtType.LoroEphemeralStore) {
      const eph = this.ensureEph();
      try {
        for (const update of updates) if (update.length > 0) eph.apply(update);
      } catch {
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      this.ack(ws, { crdt, roomId }, UpdateStatusCode.Ok, batchId);
      this.broadcast(ws, crdt, { type: MessageType.DocUpdate, crdt, roomId, updates, batchId });
      return;
    }
    this.ack(ws, { crdt, roomId }, UpdateStatusCode.Unknown, batchId);
  }

  /** Durability bookkeeping for accepted %LOR updates: buffer for the flush
   * batch, dirty the tail/backup caches, keep the daily alarm armed. */
  private recordLoroUpdates(updates: Uint8Array[]): void {
    for (const update of updates) {
      if (update.length === 0) continue;
      this.pending.push(update);
      this.pendingBytes += update.length;
    }
    this.setMeta("tailDirty", "1");
    this.setMeta("backupDirty", "1");
    this.scheduleFlush();
    this.markActivity();
  }

  private handleFragmentHeader(
    ws: WebSocket,
    state: SocketState,
    message: DocUpdateFragmentHeader
  ): void {
    if (!state.rooms.includes(message.crdt)) {
      this.ack(ws, message, UpdateStatusCode.PermissionDenied, message.batchId);
      return;
    }
    let batches = this.fragments.get(ws);
    if (!batches) {
      batches = new Map();
      this.fragments.set(ws, batches);
    }
    batches.set(message.batchId, {
      parts: Array.from({ length: message.fragmentCount }, () => new Uint8Array()),
      received: 0,
      totalSize: message.totalSizeBytes,
      header: message
    });
  }

  private handleFragment(
    ws: WebSocket,
    state: SocketState,
    message: { crdt: CrdtType; roomId: string; batchId: `0x${string}`; index: number; fragment: Uint8Array }
  ): void {
    const batch = this.fragments.get(ws)?.get(message.batchId);
    if (!batch) {
      // Unknown batch (e.g. header lost to hibernation) — tell the sender to
      // retry the whole batch.
      this.ack(ws, message, UpdateStatusCode.FragmentTimeout, message.batchId);
      return;
    }
    batch.parts[message.index] = message.fragment;
    batch.received++;
    if (batch.received < batch.parts.length) return;
    this.fragments.get(ws)?.delete(message.batchId);
    const total = new Uint8Array(batch.totalSize);
    let off = 0;
    for (const part of batch.parts) {
      total.set(part, off);
      off += part.length;
    }
    this.applyUpdates(ws, state, message.crdt, message.roomId, message.batchId, [total]);
  }

  // ── doc/ephemeral materialization ────────────────────────────────────────

  private ensureDoc(): LoroDoc {
    if (this.doc) return this.doc;
    const doc = new LoroDoc();
    const snapshot = this.blobs.get("snapshot");
    if (snapshot && snapshot.length > 0) doc.import(snapshot);
    for (const row of this.ctx.storage.sql.exec("SELECT bytes FROM updates ORDER BY seq")) {
      try {
        doc.import(new Uint8Array(row.bytes as ArrayBuffer));
      } catch {
        // A poisoned row cannot be applied; skip it rather than brick the room.
      }
    }
    for (const update of this.pending) {
      try {
        doc.import(update);
      } catch {
        /* same */
      }
    }
    this.doc = doc;
    return doc;
  }

  private ensureEph(): EphemeralStore {
    if (!this.eph) this.eph = new EphemeralStore(30_000);
    return this.eph;
  }

  // ── durability: flush, compaction, backups ───────────────────────────────

  private scheduleFlush(): void {
    if (this.flushTimer) return;
    this.flushTimer = setTimeout(() => {
      this.flushTimer = undefined;
      this.flush();
    }, DO_FLUSH_MS);
  }

  private flush(): void {
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = undefined;
    }
    if (this.pending.length === 0) return;
    const now = Date.now();
    for (const update of this.pending) {
      this.ctx.storage.sql.exec(
        "INSERT INTO updates (bytes, received_at) VALUES (?, ?)",
        update.buffer.slice(update.byteOffset, update.byteOffset + update.byteLength),
        now
      );
    }
    const logBytes = Number(this.getMeta("updateBytes") ?? "0") + this.pendingBytes;
    this.setMeta("updateBytes", String(logBytes));
    this.pending = [];
    this.pendingBytes = 0;
    if (logBytes > COMPACT_LOG_BYTES) this.foldLog();
  }

  /** LOG FOLD: full snapshot re-export + clear the update log. Lossless. */
  private foldLog(): void {
    const doc = this.ensureDoc();
    this.blobs.put("snapshot", doc.export({ mode: "snapshot" }));
    this.ctx.storage.sql.exec("DELETE FROM updates");
    this.setMeta("updateBytes", "0");
  }

  /** Daily alarm: frontier checkpoint, history trim, R2 backup. */
  async alarm(): Promise<void> {
    this.flush();
    if (this.getMeta("backupDirty") !== "1") return; // idle: stop the chain
    const doc = this.ensureDoc();
    const now = Date.now();

    // 1. Record today's frontier checkpoint.
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]") as FrontierCheckpoint[];
    checkpoints.push({
      at: now,
      frontiers: doc.frontiers().map((f) => ({ peer: String(f.peer), counter: f.counter }))
    });
    while (checkpoints.length > MAX_CHECKPOINTS) checkpoints.shift();

    // 2. HISTORY TRIM: shallow snapshot at the newest checkpoint older than
    //    RETAIN_DAYS (history before it is discarded permanently — §3.1).
    const cutoff = checkpoints.filter((c) => now - c.at >= RETAIN_MS).pop();
    if (cutoff && !(doc.isShallow() && this.getMeta("lastTrimAt") === String(cutoff.at))) {
      try {
        const shallow = doc.export({
          mode: "shallow-snapshot",
          frontiers: cutoff.frontiers.map((f) => ({ peer: f.peer as `${number}`, counter: f.counter }))
        });
        this.blobs.put("snapshot", shallow);
        this.ctx.storage.sql.exec("DELETE FROM updates");
        this.setMeta("updateBytes", "0");
        this.setMeta("lastTrimAt", String(cutoff.at));
        const fresh = new LoroDoc();
        fresh.import(shallow);
        this.doc = fresh;
      } catch {
        /* trim is best-effort; the log fold keeps the room bounded */
      }
    }
    this.setMeta("checkpoints", JSON.stringify(checkpoints));

    // 3. Nightly R2 backup (§3.3) — full current snapshot, disaster hatch.
    const chatId = this.getMeta("chatId");
    if (chatId) {
      const snapshot = (this.doc ?? doc).export({ mode: "snapshot" });
      await this.env.BLOBS.put(`backup/${chatId}/latest.loro`, snapshot);
      this.setMeta("backupDirty", "0");
    }
    // Re-arm only while there is a reason to wake again; markActivity re-arms
    // on the next write otherwise.
  }

  /** Arm the daily alarm if none is scheduled (called on every write). */
  private markActivity(): void {
    void this.ctx.storage.getAlarm().then((existing) => {
      if (existing === null) void this.ctx.storage.setAlarm(Date.now() + DAY_MS);
    });
  }

  private currentTail(): unknown {
    this.flush();
    if (this.getMeta("tailDirty") !== "1") {
      const cached = getJsonBlob<unknown>(this.blobs, "tail");
      if (cached !== undefined) return cached;
    }
    const doc = this.ensureDoc();
    const tail = materializeTail(doc, Date.now());
    putJsonBlob(this.blobs, "tail", tail);
    this.setMeta("tailDirty", "0");
    return tail;
  }

  // ── wire helpers ─────────────────────────────────────────────────────────

  private send(ws: WebSocket, message: ProtocolMessage): void {
    try {
      ws.send(encode(message));
    } catch {
      /* socket already gone; hibernation API cleans it up */
    }
  }

  /** Send updates, fragmenting any single update above the protocol cap. */
  private sendUpdates(ws: WebSocket, crdt: CrdtType, roomId: string, updates: Uint8Array[]): void {
    const small = updates.filter((u) => u.length <= MAX_MESSAGE_SIZE);
    if (small.length > 0) {
      this.send(ws, {
        type: MessageType.DocUpdate,
        crdt,
        roomId,
        updates: small,
        batchId: this.newBatchId()
      });
    }
    for (const update of updates) {
      if (update.length <= MAX_MESSAGE_SIZE) continue;
      const batchId = this.newBatchId();
      const fragmentCount = Math.ceil(update.length / FRAGMENT_BYTES);
      this.send(ws, {
        type: MessageType.DocUpdateFragmentHeader,
        crdt,
        roomId,
        batchId,
        fragmentCount,
        totalSizeBytes: update.length
      });
      for (let i = 0; i < fragmentCount; i++) {
        this.send(ws, {
          type: MessageType.DocUpdateFragment,
          crdt,
          roomId,
          batchId,
          index: i,
          fragment: update.subarray(i * FRAGMENT_BYTES, Math.min((i + 1) * FRAGMENT_BYTES, update.length))
        });
      }
    }
  }

  private broadcast(from: WebSocket, crdt: CrdtType, message: ProtocolMessage): void {
    const bytes = encode(message);
    for (const ws of this.ctx.getWebSockets()) {
      if (ws === from) continue;
      const state = ws.deserializeAttachment() as SocketState | null;
      if (!state?.rooms.includes(crdt)) continue;
      try {
        ws.send(bytes);
      } catch {
        /* stale socket */
      }
    }
  }

  private ack(
    ws: WebSocket,
    message: { crdt: CrdtType; roomId: string; batchId?: `0x${string}` },
    status: UpdateStatusCode,
    refId?: `0x${string}`
  ): void {
    this.send(ws, {
      type: MessageType.Ack,
      crdt: message.crdt,
      roomId: message.roomId,
      refId: refId ?? message.batchId ?? "0x0000000000000000",
      status
    });
  }

  private newBatchId(): `0x${string}` {
    const bytes = new Uint8Array(8);
    crypto.getRandomValues(bytes);
    return bytesToHex(bytes);
  }
}

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });
