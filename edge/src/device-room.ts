/**
 * DeviceRoom — one Durable Object per device (design §2, §8): a frame relay
 * for interactive RPC + terminal streams + future HTTP tunnel. The host keeps
 * one outbound wss; clients multiplex over `{streamId, kind, bytes}` frames —
 * a generic byte pipe from day one, transport-agnostic by construction (§8.4:
 * a WebRTC fast path could slot in under the same frames).
 *
 * Frame encoding (binary): uleb128 header-length ‖ UTF-8 JSON header ‖ payload.
 * Header: { s: streamId, k: kind, to?: connId, from?: connId }.
 * - client → DO: DO stamps `from = connId` and forwards to the host socket.
 * - host → DO: must carry `to = connId`; DO strips routing keys and delivers.
 *
 * Also holds small "sidecar" JSON slots the host publishes (repos/branches
 * snapshot for instant new-chat pickers §8.1; capability metadata) so pickers
 * render last-known state while the live RPC happens at confirm time.
 */
import { BytesReader, BytesWriter } from "loro-protocol";
import { createBlobStore, getJsonBlob, putJsonBlob, type BlobStore } from "./blobs";
import { AUTH_USER_HEADER, type Env } from "./env";

export interface DeviceFrameHeader {
  /** Stream id, unique per (connId, logical stream). */
  s: string;
  /** Stream kind: "rpc" | "term" | ... — opaque to the relay. */
  k: string;
  /** Routing: host→client target. */
  to?: string;
  /** Routing: client→host origin (stamped by the relay). */
  from?: string;
}

export const encodeDeviceFrame = (header: DeviceFrameHeader, payload: Uint8Array): Uint8Array => {
  const writer = new BytesWriter();
  writer.pushVarString(JSON.stringify(header));
  writer.pushBytes(payload);
  return writer.finalize();
};

export const decodeDeviceFrame = (
  bytes: Uint8Array
): { header: DeviceFrameHeader; payload: Uint8Array } => {
  const reader = new BytesReader(bytes);
  const header = JSON.parse(reader.readVarString()) as DeviceFrameHeader;
  const payload = reader.readBytes(reader.remaining);
  return { header, payload };
};

interface SocketState {
  userId: string;
  role: "host" | "client";
  connId: string;
}

const HOST_TAG = "host";
const clientTag = (connId: string) => `client:${connId}`;

/** Control frames the relay itself emits (kind " relay"). */
// MUST byte-match packages/rpc device-frames.ts RELAY_KIND — clients compare
// with ===; a mismatch makes host_offline/host_closed invisible to them.
const RELAY_KIND = " relay";

/** Nudge frames (§7 cold-chat command delivery): payload `{chatId}` tells the
 * host "this chat's doc has pending commands — open it and drain". Durable:
 * queued in the DO while the host is offline, replayed on its next join, so a
 * command sent to a chat the host hasn't warm-opened is never stranded. */
export const NUDGE_KIND = "nudge";
const NUDGE_MAX_PENDING = 256;
const CHAT_ID_RE = /^[A-Za-z0-9_-]{1,64}$/;

export class DeviceRoom implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly blobs: BlobStore;

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    void env;
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS pending_nudges (chat_id TEXT PRIMARY KEY, queued_at INTEGER NOT NULL)"
    );
    this.blobs = createBlobStore(ctx.storage.sql);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

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

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return new Response("unauthenticated", { status: 401 });
    const owner = this.getMeta("owner");

    if (url.pathname === "/ws") {
      const role = url.searchParams.get("role") === "host" ? "host" : "client";
      if (role === "host") {
        // The device's own backend claims the room; the claim is the identity
        // anchor every later client join is checked against.
        if (!owner) this.setMeta("owner", userId);
        else if (owner !== userId) return new Response("forbidden", { status: 403 });
      } else {
        if (!owner || owner !== userId) return new Response("forbidden", { status: 403 });
      }
      const connId = url.searchParams.get("connId") ?? crypto.randomUUID();
      const pair = new WebSocketPair();
      if (role === "host") {
        // One live host socket: close any predecessor (backend restart).
        for (const stale of this.ctx.getWebSockets(HOST_TAG)) {
          try {
            stale.close(4409, "superseded by new host connection");
          } catch {
            /* already gone */
          }
        }
        this.ctx.acceptWebSocket(pair[1], [HOST_TAG]);
        this.replayNudges(pair[1]);
      } else {
        this.ctx.acceptWebSocket(pair[1], [clientTag(connId)]);
      }
      const state: SocketState = { userId, role, connId };
      pair[1].serializeAttachment(state);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    // Sidecar slots (host-published JSON, e.g. repos snapshot §8.1).
    const sidecar = url.pathname.match(/^\/sidecar\/([a-z0-9-]{1,64})$/);
    if (sidecar) {
      const name = sidecar[1]!;
      if (!owner || owner !== userId) return json({ error: "forbidden" }, owner ? 403 : 404);
      if (request.method === "GET") {
        const value = getJsonBlob<unknown>(this.blobs, `sidecar:${name}`);
        return value === undefined ? json({ error: "not_found" }, 404) : json(value);
      }
      if (request.method === "POST") {
        putJsonBlob(this.blobs, `sidecar:${name}`, await request.json());
        return json({ ok: true });
      }
    }

    if (url.pathname === "/status" && request.method === "GET") {
      if (!owner || owner !== userId) return json({ error: "forbidden" }, owner ? 403 : 404);
      return json({ hostConnected: this.ctx.getWebSockets(HOST_TAG).length > 0 });
    }

    // Durable command nudge (§7). Any authenticated device of the owner may
    // nudge; the payload is only a chat id — the host validates against its
    // own doc before executing anything.
    if (url.pathname === "/nudge" && request.method === "POST") {
      if (!owner || owner !== userId) return json({ error: "forbidden" }, owner ? 403 : 404);
      const body = (await request.json().catch(() => null)) as { chatId?: string } | null;
      const chatId = body?.chatId;
      if (!chatId || !CHAT_ID_RE.test(chatId)) return json({ error: "bad_chat_id" }, 400);
      const host = this.ctx.getWebSockets(HOST_TAG)[0];
      if (host) {
        this.deliver(host, { s: chatId, k: NUDGE_KIND }, new TextEncoder().encode(JSON.stringify({ chatId })));
        return json({ delivered: true });
      }
      // Host offline: queue durably (dedup by chat — one open covers any
      // number of pending commands), bounded so a runaway sender can't grow
      // the DO forever. Overflow drops the OLDEST: recency wins.
      this.ctx.storage.sql.exec(
        "INSERT INTO pending_nudges (chat_id, queued_at) VALUES (?, ?) ON CONFLICT(chat_id) DO UPDATE SET queued_at = excluded.queued_at",
        chatId,
        Date.now()
      );
      this.ctx.storage.sql.exec(
        "DELETE FROM pending_nudges WHERE chat_id NOT IN (SELECT chat_id FROM pending_nudges ORDER BY queued_at DESC LIMIT ?)",
        NUDGE_MAX_PENDING
      );
      return json({ delivered: false, queued: true });
    }

    return new Response("not found", { status: 404 });
  }

  private replayNudges(host: WebSocket): void {
    const rows = [
      ...this.ctx.storage.sql.exec("SELECT chat_id FROM pending_nudges ORDER BY queued_at ASC")
    ] as Array<{ chat_id: string }>;
    if (rows.length === 0) return;
    for (const row of rows) {
      this.deliver(
        host,
        { s: row.chat_id, k: NUDGE_KIND },
        new TextEncoder().encode(JSON.stringify({ chatId: row.chat_id }))
      );
    }
    this.ctx.storage.sql.exec("DELETE FROM pending_nudges");
  }

  webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): void {
    if (typeof message === "string") return; // ping/pong auto-response
    const state = ws.deserializeAttachment() as SocketState;
    let frame: { header: DeviceFrameHeader; payload: Uint8Array };
    try {
      frame = decodeDeviceFrame(new Uint8Array(message));
    } catch {
      ws.close(1002, "Frame error");
      return;
    }
    if (state.role === "client") {
      const host = this.ctx.getWebSockets(HOST_TAG)[0];
      if (!host) {
        // Host offline: bounce a relay-level error so the client can surface
        // "device is asleep" instead of hanging.
        this.deliver(ws, { s: frame.header.s, k: RELAY_KIND }, encodeRelayError("host_offline"));
        return;
      }
      this.deliver(host, { s: frame.header.s, k: frame.header.k, from: state.connId }, frame.payload);
      return;
    }
    // Host frame: route by `to`.
    const to = frame.header.to;
    if (!to) return;
    const target = this.ctx.getWebSockets(clientTag(to))[0];
    if (!target) {
      this.deliver(ws, { s: frame.header.s, k: RELAY_KIND, to }, encodeRelayError("client_gone"));
      return;
    }
    this.deliver(target, { s: frame.header.s, k: frame.header.k }, frame.payload);
  }

  webSocketClose(ws: WebSocket): void {
    const state = ws.deserializeAttachment() as SocketState | null;
    if (!state) return;
    if (state.role === "client") {
      // Tell the host so it can tear down any per-client streams (ptys etc.).
      const host = this.ctx.getWebSockets(HOST_TAG)[0];
      if (host) {
        this.deliver(host, { s: "", k: RELAY_KIND, from: state.connId }, encodeRelayError("client_closed"));
      }
      return;
    }
    // Host went away: notify every client.
    for (const client of this.ctx.getWebSockets()) {
      const cs = client.deserializeAttachment() as SocketState | null;
      if (cs?.role !== "client") continue;
      this.deliver(client, { s: "", k: RELAY_KIND }, encodeRelayError("host_closed"));
    }
  }

  webSocketError(ws: WebSocket): void {
    this.webSocketClose(ws);
  }

  private deliver(ws: WebSocket, header: DeviceFrameHeader, payload: Uint8Array): void {
    try {
      ws.send(encodeDeviceFrame(header, payload));
    } catch {
      /* stale socket */
    }
  }
}

const encodeRelayError = (code: string): Uint8Array =>
  new TextEncoder().encode(JSON.stringify({ error: code }));

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });
