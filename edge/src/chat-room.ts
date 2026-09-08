/**
 * ChatRoom — one Durable Object per chat session (`chat2/{chatId}`), the
 * dumb authenticated log relay replacing SessionRoom's loro-aware s2 rooms
 * (docs/chat2-sync.md workstream B). Modeled line-for-line on RegistryRoom,
 * NOT on SessionRoom: no loro-wasm import anywhere in this class.
 *
 * The DO's entire job: append opaque update blobs to a seq-ordered log,
 * relay them to live sockets, store one client-built checkpoint blob, and
 * serve both back. All CRDT semantics live in the clients. Cold start is a
 * table read — the s2 wedge class (CPU-limited export/replay in the DO)
 * cannot exist here by construction.
 *
 * Sidecars (`/tail`, `/diff`) are host-PUBLISHED and served verbatim: the DO
 * never materializes anything.
 *
 * Hibernation discipline: ZERO wall-clock timers; ping/pong rides the
 * auto-response pair; the daily alarm does the nightly R2 backup only.
 */
import { createBlobStore, type BlobStore } from "./blobs";
import {
  appendRow,
  CHECKPOINT_BLOB,
  commitCheckpoint,
  ensureChatLog,
  FRONTIER_BLOB,
  getMeta,
  headSeq,
  logStats,
  MAX_ROW_BYTES,
  rowsAfter,
  setMeta
} from "./chat-log";
import { decodeFrame, encodeFrame, FRAME } from "./chat-frames";
import { AUTH_ORG_HEADER, AUTH_USER_HEADER, ENCRYPTED_ROOM_HEADER, type Env } from "./env";
import { profileRequiresEncryption } from "./vault-gate";
import { looksLikeSealedContent } from "./vault-records";

const DAY_MS = 24 * 60 * 60 * 1000;
/** Inbound frame budget: one pushed row (+ header slack). */
const MAX_FRAME_BYTES = MAX_ROW_BYTES + 8192;
/** Host-published sidecar budget (tail JSON / diff payload). */
const MAX_SIDECAR_BYTES = 4 * 1024 * 1024;
/** Checkpoint upload budget — a REBUILT thin doc is ~KB-to-100s-of-KB scale;
 * 16MB is deliberate headroom over any sane doc, not a target. */
const MAX_CHECKPOINT_BYTES = 16 * 1024 * 1024;
/** Presence beats older than this are swept before relay/stats. */
const PRESENCE_TTL_MS = 30_000;
/** Per-device push quota, rolling window (in-memory; resets on hibernation —
 * it exists to contain a runaway client loop, not to meter honest traffic). */
const QUOTA_WINDOW_MS = 60_000;
const QUOTA_MAX_PUSHES = 300;
const QUOTA_MAX_BYTES = 8 * 1024 * 1024;

interface SocketState {
  userId: string;
  orgId?: string;
  device: string;
  /** Set once a valid hello established the session. */
  ready?: boolean;
  /** Encrypted room generation: pushed rows must be signed content records. */
  encrypted?: boolean;
}

interface PushOutcome {
  ok: number;
  rejected: number;
  lastOkAt: number;
}

interface QuotaWindow {
  since: number;
  pushes: number;
  bytes: number;
}

export class ChatRoom implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;
  private readonly blobs: BlobStore;
  /** device → last presence beat (epoch ms). Memory-only by construction. */
  private readonly presence = new Map<string, number>();
  /** device → rolling push quota window. Memory-only. */
  private readonly quotas = new Map<string, QuotaWindow>();

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    ensureChatLog(ctx.storage.sql);
    this.blobs = createBlobStore(ctx.storage.sql);
    // Runtime-answered keepalive; proves nothing about this DO's health.
    // Clients judge liveness by probe frames (same caveat as RegistryRoom).
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  // ── HTTP surface (only reachable through the authed Worker) ──────────────

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return json({ error: "unauthenticated" }, 401);

    const sql = this.ctx.storage.sql;
    const owner = getMeta(sql, "owner");
    // Worker-stamped (never client-derived): this room is an encrypted
    // generation, so every content body must be ciphertext-shaped — a
    // client that regressed to plaintext is refused, not stored (RFC §12.2).
    const encrypted = request.headers.get(ENCRYPTED_ROOM_HEADER) === "1";
    if (encrypted && getMeta(sql, "encrypted") !== "1") setMeta(sql, "encrypted", "1");
    const sealedOnly = encrypted || getMeta(sql, "encrypted") === "1";

    if (url.pathname === "/ws") {
      // Claim-on-first-join ownership, then owner-only forever (the s2
      // discipline: chat ids are client-minted, the first authed user to
      // dial one owns it).
      if (!owner) setMeta(sql, "owner", userId);
      else if (owner !== userId) return json({ error: "forbidden" }, 403);
      const device = url.searchParams.get("device") ?? "";
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1]);
      const state: SocketState = { userId, device, encrypted: sealedOnly, orgId: request.headers.get(AUTH_ORG_HEADER) ?? undefined };
      pair[1].serializeAttachment(state);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    if (url.pathname === "/checkpoint" && request.method === "POST") {
      if (!owner) setMeta(sql, "owner", userId);
      else if (owner !== userId) return json({ error: "forbidden" }, 403);
      const seqCovered = Number(url.searchParams.get("seqCovered") ?? "");
      if (!Number.isInteger(seqCovered) || seqCovered < 0) {
        return json({ error: "bad_seq_covered" }, 400);
      }
      const frontier = decodeBase64(request.headers.get("x-chat2-frontier") ?? "");
      if (frontier === undefined) return json({ error: "bad_frontier" }, 400);
      // A checkpoint that claims to cover rows must name its state: an empty
      // frontier label on a content-bearing checkpoint made every fresh
      // reader skip it and park all dependent rows invisibly ("Add Tweets"
      // incident, 2026-08-18). Empty stays legal only for seqCovered 0
      // (the M1 empty-doc seed).
      if (frontier.byteLength === 0 && seqCovered > 0) {
        return json({ error: "bad_frontier", message: "empty frontier on a content checkpoint" }, 400);
      }
      const body = new Uint8Array(await request.arrayBuffer());
      if (body.byteLength > MAX_CHECKPOINT_BYTES) return json({ error: "too_large" }, 413);
      if (
        sealedOnly &&
        (!looksLikeSealedContent(body, MAX_CHECKPOINT_BYTES, 2n) ||
          !looksLikeSealedContent(frontier, 65536, 3n))
      ) {
        return json({ error: "plaintext_rejected" }, 400);
      }
      const outcome = commitCheckpoint(sql, this.blobs, seqCovered, frontier, body, Date.now());
      if (!outcome.ok) return json({ error: outcome.error }, 409);
      this.markBackupDirty();
      return json({ ok: true, seqFloor: outcome.seqFloor, pruned: outcome.pruned });
    }

    // Claim-on-first-contact for the HTTP surface too — same client-minted-id
    // discipline as /ws. The /rows twins predate the pull-first HTTPS
    // transport and 404'd an unclaimed room, which deadlocked a brand-new
    // chat on WS-hostile networks: the sender's push 404s, the host's pull
    // 404s, and the only claimants (WS join, checkpoint heal) never run.
    if (!owner) setMeta(sql, "owner", userId);
    else if (owner !== userId) return json({ error: "forbidden" }, 403);

    if (url.pathname === "/checkpoint" && request.method === "GET") {
      const bytes = this.blobs.get(CHECKPOINT_BLOB);
      if (!bytes) return json({ error: "not_found" }, 404);
      // Range-resumable (bytes=N- only): a 1MB load over a 1.2Mbps link that
      // dies at byte 800k resumes, where s2's export-per-join restarted.
      const range = parseRangeStart(request.headers.get("range"));
      if (range !== null && range >= bytes.byteLength) {
        return new Response(null, {
          status: 416,
          headers: { "content-range": `bytes */${bytes.byteLength}` }
        });
      }
      const body = range !== null ? bytes.subarray(range) : bytes;
      const headers = new Headers({
        "content-type": "application/octet-stream",
        "content-length": String(body.byteLength),
        "accept-ranges": "bytes",
        "x-chat2-checkpoint-seq": getMeta(sql, "checkpointSeq") ?? "0"
      });
      if (range !== null) {
        headers.set(
          "content-range",
          `bytes ${range}-${bytes.byteLength - 1}/${bytes.byteLength}`
        );
      }
      return new Response(body, { status: range !== null ? 206 : 200, headers });
    }

    if (url.pathname === "/rows" && request.method === "GET") {
      // Pull over plain HTTPS: one GET collapses connect → hello → state →
      // rowsReq → backfill (4+ serial round trips on a WS, and impossible on
      // networks that strip the upgrade) into a single request. The body is
      // u32-LE length-prefixed frames — state (frontier payload), rows after
      // `?after=`, rowsDone — byte-identical frame encoding to the WS path,
      // so clients reuse their existing decoder.
      const afterRaw = Number(url.searchParams.get("after") ?? "0");
      const after = Number.isInteger(afterRaw) && afterRaw >= 0 ? afterRaw : 0;
      const device = url.searchParams.get("device") ?? "";
      const exclude =
        url.searchParams.get("excludeOwn") === "1" && device !== "" ? device : undefined;
      const stats = logStats(sql);
      const frontier = this.blobs.get(FRONTIER_BLOB) ?? new Uint8Array(0);
      const frames: Uint8Array[] = [
        encodeFrame(
          FRAME.state,
          {
            headSeq: stats.headSeq,
            seqFloor: stats.seqFloor,
            checkpointSeq: stats.checkpointSeq,
            checkpointSize: stats.checkpointSize,
            rowCount: stats.rowCount,
            rowBytes: stats.rowBytes
          },
          frontier
        )
      ];
      // Response cap: the WS path streams; this buffers, so bound the body.
      // Past the cap the response ends WITHOUT rowsDone — clients apply what
      // arrived, their cursor advances per row, and the next pull resumes
      // from there (pagination by truncation).
      const ROWS_BODY_CAP = 4 * 1024 * 1024;
      let bodyBytes = 0;
      let truncated = false;
      for (const row of rowsAfter(sql, after, exclude)) {
        const frame = encodeFrame(
          FRAME.row,
          { seq: row.seq, device: row.device, batchId: row.batchId },
          row.bytes
        );
        bodyBytes += 4 + frame.length;
        if (bodyBytes > ROWS_BODY_CAP) {
          truncated = true;
          break;
        }
        frames.push(frame);
      }
      if (!truncated) {
        frames.push(encodeFrame(FRAME.rowsDone, { headSeq: headSeq(sql) }));
      }
      const total = frames.reduce((n, f) => n + 4 + f.length, 0);
      const body = new Uint8Array(total);
      const view = new DataView(body.buffer);
      let off = 0;
      for (const f of frames) {
        view.setUint32(off, f.length, true);
        body.set(f, off + 4);
        off += 4 + f.length;
      }
      return new Response(body, {
        headers: { "content-type": "application/octet-stream" }
      });
    }

    if (url.pathname === "/rows" && request.method === "POST") {
      // Push over plain HTTPS — the WS push's fallback twin for networks
      // where the upgrade never completes. batchId dedupe (UNIQUE column)
      // makes at-least-once delivery exact-once in effect.
      const device = url.searchParams.get("device") ?? "";
      const batchId = url.searchParams.get("batchId") ?? "";
      if (batchId === "" || batchId.length > 128) {
        this.recordPush(device, false);
        return json({ error: "bad_push" }, 400);
      }
      // Pre-read cap (the WS runtime closes 1 MiB messages before the DO
      // runs; HTTP needs the explicit twin). appendRow re-checks post-read.
      const declared = Number(request.headers.get("content-length") ?? "0");
      if (declared > MAX_ROW_BYTES + 4096) {
        this.recordPush(device, false);
        return json({ error: "too_large" }, 413);
      }
      const payload = new Uint8Array(await request.arrayBuffer());
      if (sealedOnly && !looksLikeSealedContent(payload, MAX_ROW_BYTES, 1n)) {
        this.recordPush(device, false);
        return json({ error: "plaintext_rejected" }, 400);
      }
      if (!this.admitQuota(device, payload.byteLength)) {
        this.recordPush(device, false);
        return json({ error: "quota" }, 429);
      }
      const outcome = appendRow(sql, device, batchId, payload, Date.now());
      if (!outcome.ok) {
        this.recordPush(device, false);
        return json({ error: outcome.error }, outcome.error === "too_large" ? 413 : 400);
      }
      this.recordPush(device, true);
      if (!outcome.dup) {
        this.markBackupDirty();
        // Live relay to every ready socket — a same-device socket would
        // re-import its own bytes as a Loro no-op, so no exclusion needed.
        for (const socket of this.ctx.getWebSockets()) {
          const socketState = socket.deserializeAttachment() as SocketState | null;
          if (!socketState?.ready) continue;
          send(socket, FRAME.row, { seq: outcome.seq, device, batchId }, payload);
        }
      }
      return json({ batchId, seq: outcome.seq, dup: outcome.dup });
    }

    if ((url.pathname === "/tail" || url.pathname === "/diff") && request.method === "PUT") {
      const name = url.pathname === "/tail" ? "sidecar-tail" : "sidecar-diff";
      const body = new Uint8Array(await request.arrayBuffer());
      if (body.byteLength > MAX_SIDECAR_BYTES) return json({ error: "too_large" }, 413);
      if (sealedOnly && !looksLikeSealedContent(body, MAX_SIDECAR_BYTES, url.pathname === "/tail" ? 5n : 6n)) {
        return json({ error: "plaintext_rejected" }, 400);
      }
      this.blobs.put(name, body);
      setMeta(sql, `${name}-type`, request.headers.get("content-type") ?? "application/json");
      return json({ ok: true, bytes: body.byteLength });
    }

    if ((url.pathname === "/tail" || url.pathname === "/diff") && request.method === "GET") {
      const name = url.pathname === "/tail" ? "sidecar-tail" : "sidecar-diff";
      const bytes = this.blobs.get(name);
      if (!bytes) return json({ error: "not_found" }, 404);
      return new Response(bytes, {
        headers: {
          "content-type": getMeta(sql, `${name}-type`) ?? "application/json",
          "content-length": String(bytes.byteLength)
        }
      });
    }

    if (url.pathname === "/stats" && request.method === "GET") {
      this.sweepPresence();
      return json({
        ...logStats(sql),
        connectedSockets: this.ctx.getWebSockets().length,
        presence: Object.fromEntries(this.presence),
        // The ONLY per-device attribution surface — kept from the 2026-08-05
        // incident tooling (SessionRoom's /stats pushOutcomes).
        pushOutcomes: JSON.parse(getMeta(sql, "pushOutcomes") ?? "{}") as Record<
          string,
          PushOutcome
        >,
        lastBackupSeq: Number(getMeta(sql, "backupSeq") ?? "0")
      });
    }

    if (url.pathname === "/reset" && request.method === "POST") {
      // Operator wipe. Recovery is host-driven: the host detects
      // `headSeq < cursor` on its next hello and re-seeds via checkpoint —
      // same shape as the registry reset recipe.
      sql.exec("DELETE FROM rows");
      sql.exec("DELETE FROM meta");
      sql.exec("DELETE FROM blobs");
      for (const ws of this.ctx.getWebSockets()) {
        try {
          ws.close(4410, "chat room reset");
        } catch {
          /* already gone */
        }
      }
      return json({ ok: true });
    }

    return json({ error: "not found" }, 404);
  }

  // ── WebSocket protocol (binary frames, chat-frames.ts) ───────────────────

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    if (typeof message === "string") {
      ws.close(1003, "binary frames only");
      return;
    }
    if (message.byteLength > MAX_FRAME_BYTES) {
      ws.close(1009, "frame too large");
      return;
    }
    const frame = decodeFrame(new Uint8Array(message));
    const state = ws.deserializeAttachment() as SocketState;
    if (!frame) {
      send(ws, FRAME.error, { code: "bad_frame", message: "malformed frame" });
      return;
    }
    switch (frame.type) {
      case FRAME.hello:
        this.handleHello(ws, state, frame.header);
        return;
      case FRAME.rowsReq:
        this.handleRowsReq(ws, state, frame.header);
        return;
      case FRAME.push:
        if (!state.encrypted && await profileRequiresEncryption(this.env, state.orgId, state.userId)) {
          send(ws, FRAME.error, { code: "encrypted_profile", message: "plaintext writes refused", batchId: frame.header.batchId });
          ws.close(4403, "encrypted profile");
          return;
        }
        this.handlePush(ws, state, frame.header, frame.payload);
        return;
      case FRAME.presence:
        this.handlePresence(ws, state, frame.header, frame.payload);
        return;
      case FRAME.probe:
        send(ws, FRAME.probeOk, { headSeq: headSeq(this.ctx.storage.sql) });
        return;
      default:
        send(ws, FRAME.error, { code: "bad_frame", message: `unexpected type ${frame.type}` });
    }
  }

  async webSocketClose(): Promise<void> {
    /* nothing buffered; rows are written synchronously on push */
  }

  async webSocketError(): Promise<void> {
    /* ditto */
  }

  private handleHello(ws: WebSocket, state: SocketState, header: Record<string, unknown>): void {
    if (typeof header.device === "string" && header.device.length > 0) {
      state.device = header.device;
    }
    state.ready = true;
    ws.serializeAttachment(state);
    const sql = this.ctx.storage.sql;
    const stats = logStats(sql);
    const frontier = this.blobs.get(FRONTIER_BLOB) ?? new Uint8Array(0);
    // Metadata + frontier only — the CLIENT decides what to load next
    // (frontier included in local doc → rows only; not included → GET
    // /checkpoint first; cursor > headSeq → server lost state, re-seed).
    send(
      ws,
      FRAME.state,
      {
        headSeq: stats.headSeq,
        seqFloor: stats.seqFloor,
        checkpointSeq: stats.checkpointSeq,
        checkpointSize: stats.checkpointSize,
        rowCount: stats.rowCount,
        rowBytes: stats.rowBytes
      },
      frontier
    );
  }

  private handleRowsReq(ws: WebSocket, state: SocketState, header: Record<string, unknown>): void {
    if (!state.ready) {
      send(ws, FRAME.error, { code: "hello_first", message: "rows before hello" });
      return;
    }
    const after = typeof header.after === "number" && header.after >= 0 ? header.after : 0;
    const exclude = header.excludeOwn === true ? state.device : undefined;
    const sql = this.ctx.storage.sql;
    for (const row of rowsAfter(sql, after, exclude)) {
      send(ws, FRAME.row, { seq: row.seq, device: row.device, batchId: row.batchId }, row.bytes);
    }
    send(ws, FRAME.rowsDone, { headSeq: headSeq(sql) });
  }

  private handlePush(
    ws: WebSocket,
    state: SocketState,
    header: Record<string, unknown>,
    payload: Uint8Array
  ): void {
    const batchId = typeof header.batchId === "string" ? header.batchId : "";
    // Push errors carry the batchId so clients can RETIRE permanently
    // rejected batches from their replay queues (an unretireable batch
    // replays on every reconnect forever — the wedge class this replaces).
    if (!state.ready || batchId === "" || batchId.length > 128) {
      this.recordPush(state.device, false);
      send(ws, FRAME.error, { code: "bad_push", message: "hello first / malformed push", batchId });
      return;
    }
    if (state.encrypted && !looksLikeSealedContent(payload, MAX_ROW_BYTES, 1n)) {
      // Permanent for this batch: the client must not replay plaintext.
      this.recordPush(state.device, false);
      send(ws, FRAME.error, { code: "plaintext_rejected", message: "encrypted room: signed content records only", batchId });
      return;
    }
    if (!this.admitQuota(state.device, payload.byteLength)) {
      this.recordPush(state.device, false);
      send(ws, FRAME.error, { code: "quota", message: "per-device push quota exceeded", batchId });
      return;
    }
    const sql = this.ctx.storage.sql;
    const outcome = appendRow(sql, state.device, batchId, payload, Date.now());
    if (!outcome.ok) {
      this.recordPush(state.device, false);
      send(ws, FRAME.error, {
        code: outcome.error,
        message: `push rejected: ${outcome.error}`,
        batchId
      });
      return;
    }
    this.recordPush(state.device, true);
    if (!outcome.dup) {
      this.markBackupDirty();
      // Live relay to every OTHER ready socket — the sender has its own
      // bytes; it gets the ack (contrast RegistryRoom, whose LWW merge means
      // the sender must see the merged truth — here bytes are opaque and
      // Loro convergence is the client's business).
      for (const socket of this.ctx.getWebSockets()) {
        if (socket === ws) continue;
        const socketState = socket.deserializeAttachment() as SocketState | null;
        if (!socketState?.ready) continue;
        send(
          socket,
          FRAME.row,
          { seq: outcome.seq, device: state.device, batchId },
          payload
        );
      }
    }
    send(ws, FRAME.ack, { batchId, seq: outcome.seq, dup: outcome.dup });
  }

  private handlePresence(
    ws: WebSocket,
    state: SocketState,
    header: Record<string, unknown>,
    payload: Uint8Array
  ): void {
    if (!state.ready || state.device === "") return;
    const at = typeof header.at === "number" ? header.at : Date.now();
    this.presence.set(state.device, at);
    this.sweepPresence();
    // Broadcast-only relay of the opaque payload — no EphemeralStore, no
    // storage; a device that joins later simply waits for the next beat.
    for (const socket of this.ctx.getWebSockets()) {
      if (socket === ws) continue;
      const socketState = socket.deserializeAttachment() as SocketState | null;
      if (!socketState?.ready) continue;
      send(socket, FRAME.presence, { device: state.device, at }, payload);
    }
  }

  private sweepPresence(): void {
    const horizon = Date.now() - PRESENCE_TTL_MS;
    for (const [device, at] of this.presence) {
      if (at < horizon) this.presence.delete(device);
    }
  }

  /** Rolling per-device quota. True = admitted. */
  private admitQuota(device: string, bytes: number): boolean {
    const now = Date.now();
    const key = device === "" ? "(unknown)" : device;
    let window = this.quotas.get(key);
    if (!window || now - window.since > QUOTA_WINDOW_MS) {
      window = { since: now, pushes: 0, bytes: 0 };
      this.quotas.set(key, window);
    }
    window.pushes += 1;
    window.bytes += bytes;
    return window.pushes <= QUOTA_MAX_PUSHES && window.bytes <= QUOTA_MAX_BYTES;
  }

  private recordPush(device: string, ok: boolean): void {
    const sql = this.ctx.storage.sql;
    const key = device === "" ? "(unknown)" : device;
    const outcomes = JSON.parse(getMeta(sql, "pushOutcomes") ?? "{}") as Record<
      string,
      PushOutcome
    >;
    const entry = outcomes[key] ?? { ok: 0, rejected: 0, lastOkAt: 0 };
    if (ok) {
      entry.ok += 1;
      entry.lastOkAt = Date.now();
    } else {
      entry.rejected += 1;
    }
    outcomes[key] = entry;
    setMeta(sql, "pushOutcomes", JSON.stringify(outcomes));
  }

  private markBackupDirty(): void {
    setMeta(this.ctx.storage.sql, "backupDirty", "1");
    void this.ctx.storage.getAlarm().then((existing) => {
      if (existing === null) void this.ctx.storage.setAlarm(Date.now() + DAY_MS);
    });
  }

  /** Daily alarm: nightly R2 backup, seq-monotonic so a reset-and-reseeding
   * room can never replace the last good copy with a hollow one. */
  async alarm(): Promise<void> {
    const sql = this.ctx.storage.sql;
    if (getMeta(sql, "backupDirty") !== "1") return; // idle: stop the chain
    const head = headSeq(sql);
    if (head > Number(getMeta(sql, "backupSeq") ?? "0")) {
      const rows = [...rowsAfter(sql, 0)].map((row) => ({
        seq: row.seq,
        device: row.device,
        batchId: row.batchId,
        bytes: encodeBase64(row.bytes)
      }));
      const checkpoint = this.blobs.get(CHECKPOINT_BLOB);
      const frontier = this.blobs.get(FRONTIER_BLOB);
      await this.env.BLOBS.put(
        `backup/chat2/${this.ctx.id.toString()}/latest.json`,
        JSON.stringify({
          at: Date.now(),
          ...logStats(sql),
          checkpoint: checkpoint ? encodeBase64(checkpoint) : null,
          frontier: frontier ? encodeBase64(frontier) : null,
          rows
        })
      );
      setMeta(sql, "backupSeq", String(head));
    }
    setMeta(sql, "backupDirty", "0");
  }
}

const send = (
  ws: WebSocket,
  type: (typeof FRAME)[keyof typeof FRAME],
  header: Record<string, unknown>,
  payload?: Uint8Array
): void => {
  try {
    const frame = encodeFrame(type, header, payload);
    ws.send(frame.buffer.slice(frame.byteOffset, frame.byteOffset + frame.byteLength) as ArrayBuffer);
  } catch {
    /* socket already gone; hibernation API cleans it up */
  }
};

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

/** `bytes=N-` (open-ended resume) only; anything fancier is ignored → 200. */
const parseRangeStart = (header: string | null): number | null => {
  const match = header?.match(/^bytes=(\d+)-$/);
  if (!match) return null;
  const start = Number(match[1]);
  return Number.isSafeInteger(start) && start > 0 ? start : null;
};

const encodeBase64 = (bytes: Uint8Array): string => {
  let bin = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(bin);
};

/** Standard base64 (empty string ⇒ empty frontier). `undefined` = malformed. */
const decodeBase64 = (text: string): Uint8Array | undefined => {
  try {
    const bin = atob(text);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  } catch {
    return undefined;
  }
};
