/**
 * Zeron-native edge Worker (design §2, ARCHITECTURE §6): JWT auth at the
 * edge, then forwarding into per-session, per-workspace, and per-device
 * Durable Objects. Also serves content-addressed R2 attachments (§1.2) and
 * the absorbed WorkOS auth routes (formerly apps/server).
 *
 * Routes:
 *   GET  /health
 *   POST /auth/exchange               — WorkOS code → tokens
 *   POST /auth/refresh                — WorkOS refresh → fresh tokens
 *   GET  /auth/orgs                   — caller's active org memberships
 *   POST /auth/orgs                   — create org + admin membership
 *   GET  /auth/cli/callback           — headless sign-in paste-code page
 *   GET  /session/:chatId/ws          — loro-protocol room (wss upgrade)
 *   GET  /tail/:chatId                — L2 instant-open tail JSON (§5)
 *   GET  /diff/:chatId                — latest working-tree diff (§6.1)
 *   POST /diff/:chatId                — host publishes the diff sidecar
 *   GET  /snapshot/:chatId            — repair: read current doc snapshot
 *   POST /append/:chatId              — repair: merge-import a Loro update
 *   GET  /workspace/:orgId/ws         — workspace-doc room `ws/{orgId}` (wss; legacy clients)
 *   GET  /workspace/:orgId/tail       — workspace-doc tail JSON
 *   GET  /registry/:orgId/ws          — workspace registry room `reg1/{orgId}/{user}` (wss)
 *   GET  /registry/:orgId/stats       — registry seq/rows/attribution
 *   GET  /registry/:orgId/rows        — registry full-table repair read
 *   POST /registry/:orgId/reset       — registry operator wipe (self-healing)
 *   GET  /device/:deviceId/ws?role=   — device-room byte pipe (§8)
 *   GET  /device/:deviceId/sidecar/:name
 *   POST /device/:deviceId/sidecar/:name
 *   GET  /device/:deviceId/status
 *   PUT  /blob/:chatId/:partId        — tool-output sidecar (chat2-sync A2)
 *   GET  /blob/:chatId/:partId
 *   GET  /chat2/:chatId/ws            — chat2 log-relay room (wss, chat2-sync B)
 *   GET|POST /chat2/:chatId/checkpoint — client-built doc snapshot (Range-resumable GET)
 *   GET|PUT  /chat2/:chatId/tail      — host-published sidecars, served verbatim
 *   GET|PUT  /chat2/:chatId/diff
 *   GET  /chat2/:chatId/stats
 *   POST /chat2/:chatId/reset
 *   ANY  /vault/:orgId/*              — encrypted-sync control plane `vault1/{orgId}/{user}`
 */
import { authenticate } from "./auth";
import { handleAuthRoute } from "./auth-routes";
import { AUTH_USER_HEADER, ROOM_KIND_HEADER, type Env } from "./env";
import { SessionRoom } from "./session-room";
import { DeviceRoom } from "./device-room";
import { RegistryRoom } from "./registry-room";
import { ChatRoom } from "./chat-room";
import { VaultRoom } from "./vault-room";
import installSh from "./install.sh";

export { SessionRoom, DeviceRoom, RegistryRoom, ChatRoom, VaultRoom };

const ID_RE = /^[A-Za-z0-9_-]{1,128}$/;

/** `decodeURIComponent` that answers `undefined` for malformed %-escapes. */
const safeDecode = (segment: string): string | undefined => {
  try {
    return decodeURIComponent(segment);
  } catch {
    return undefined;
  }
};
/** Tool part ids are harness-minted (`tool-1`, `call_x`, `m1#c1`-style) —
 * wider than ID_RE but still no slashes, so a part id can't traverse keys. */
const PART_RE = /^[A-Za-z0-9._:#~-]{1,200}$/;
const MAX_TOOL_BLOB_BYTES = 1024 * 1024;

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

/** Forward into a DO with the verified user stamped on the request. */
const forward = (
  ns: DurableObjectNamespace,
  name: string,
  request: Request,
  userId: string,
  path: string,
  search?: string,
  roomKind?: "workspace"
): Promise<Response> => {
  const stub = ns.get(ns.idFromName(name));
  const url = new URL(request.url);
  url.pathname = path;
  if (search !== undefined) url.search = search;
  const headers = new Headers(request.headers);
  // room-kind is a Worker-controlled signal (the DO relaxes owner gating for
  // workspace rooms): clear any inbound value so only the explicit set below —
  // reached solely on workspace forwards, after the org-membership check —
  // can assert it. Do not drop this line; passthrough would let a caller
  // choose their own room kind.
  headers.delete(ROOM_KIND_HEADER);
  headers.set(AUTH_USER_HEADER, userId);
  if (roomKind) headers.set(ROOM_KIND_HEADER, roomKind);
  return stub.fetch(new Request(url.toString(), { ...requestInit(request), headers }));
};

const requestInit = (request: Request): RequestInit => ({
  method: request.method,
  body: request.body
});

/** Carry the dialing engine's `&device=` through to the DO (socket
 * attribution in logs — the 2026-08-04 deaf socket was only identifiable by
 * reverse-engineering rotating IPv6 privacy addresses). Validated so a
 * hand-crafted value can't inject into log lines or the DO's query. */
const deviceParam = (url: URL): string => {
  const device = url.searchParams.get("device") ?? "";
  return ID_RE.test(device) ? `&device=${device}` : "";
};

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const parts = url.pathname.split("/").filter(Boolean);

    if (url.pathname === "/health") {
      return json({ ok: true, auth: env.AUTH_MODE === "dev" ? "dev" : "workos" });
    }

    // ── public install surface (also routed from zeron.sh): the
    //    `curl | sh` installer and the release artifacts it downloads ───────
    if (url.pathname === "/install.sh" && (request.method === "GET" || request.method === "HEAD")) {
      return new Response(request.method === "HEAD" ? null : installSh, {
        headers: {
          "content-type": "application/x-sh",
          "cache-control": "public, max-age=0, must-revalidate"
        }
      });
    }
    if (
      parts[0] === "releases" &&
      parts.length >= 2 &&
      (request.method === "GET" || request.method === "HEAD")
    ) {
      const key = decodeURIComponent(url.pathname.slice("/releases/".length));
      if (key.length === 0 || key.includes("..")) return json({ error: "bad request" }, 400);
      const object = await env.RELEASES.get(key);
      if (!object) return json({ error: "not_found" }, 404);
      // latest.txt / manifest.json flip on release; artifacts are immutable by name.
      const mutable = key.endsWith(".txt") || key.endsWith(".json");
      const headers = new Headers({
        "content-type": key.endsWith(".txt")
          ? "text/plain; charset=utf-8"
          : key.endsWith(".json")
            ? "application/json"
            : "application/octet-stream",
        "content-length": String(object.size),
        "cache-control": mutable ? "public, max-age=60" : "public, max-age=86400, immutable",
        etag: object.httpEtag
      });
      return new Response(request.method === "HEAD" ? null : object.body, { headers });
    }

    // ── WorkOS auth routes (pre-bearer: exchange/refresh/callback have no
    //    access token yet; the org routes verify the bearer themselves) ─────
    const authRouted = await handleAuthRoute(request, env, url);
    if (authRouted) return authRouted;

    const auth = await authenticate(env, request);
    if (!auth) return json({ error: "unauthenticated" }, 401);

    // ── session rooms ───────────────────────────────────────────────────────
    if (parts[0] === "session" && parts[1] && ID_RE.test(parts[1]) && parts[2] === "ws") {
      if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
        return json({ error: "expected websocket" }, 426);
      }
      // `s2/` = the WorkOS staging→production identity break: rooms are
      // claim-on-first-join per user id, and prod issued a fresh id for
      // everyone — a new namespace lets prod identities claim fresh rooms
      // while hosts re-upload doc state from their local snapshots (same
      // playbook as `ws3` below). Frame-level room ids stay the bare chatId.
      return forward(
        env.SESSION_ROOMS,
        `s2/${parts[1]}`,
        request,
        auth.userId,
        "/ws",
        `?chatId=${parts[1]}${deviceParam(url)}`
      );
    }
    if (parts[0] === "tail" && parts[1] && ID_RE.test(parts[1]) && request.method === "GET") {
      return forward(env.SESSION_ROOMS, `s2/${parts[1]}`, request, auth.userId, "/tail", "");
    }
    if (parts[0] === "stats" && parts[1] && ID_RE.test(parts[1]) && request.method === "GET") {
      return forward(env.SESSION_ROOMS, `s2/${parts[1]}`, request, auth.userId, "/stats", "");
    }
    if (parts[0] === "diff" && parts[1] && ID_RE.test(parts[1])) {
      return forward(env.SESSION_ROOMS, `s2/${parts[1]}`, request, auth.userId, "/diff", "");
    }
    if (parts[0] === "snapshot" && parts[1] && ID_RE.test(parts[1]) && request.method === "GET") {
      return forward(env.SESSION_ROOMS, `s2/${parts[1]}`, request, auth.userId, "/snapshot", "");
    }
    if (parts[0] === "append" && parts[1] && ID_RE.test(parts[1]) && request.method === "POST") {
      return forward(env.SESSION_ROOMS, `s2/${parts[1]}`, request, auth.userId, "/append", "");
    }

    // ── chat2 rooms (docs/chat2-sync.md B): dumb log relays, one per chat.
    //    Claim-on-first-join ownership enforced in the DO (chat ids are
    //    client-minted). The DO handles /ws, /checkpoint (GET Range-resumable
    //    + POST floor-guarded), host-published /tail + /diff sidecars,
    //    /stats, /reset. ──────────────────────────────────────────────────────
    if (parts[0] === "chat2" && parts[1] && ID_RE.test(parts[1]) && parts[2]) {
      const room = `chat2/${parts[1]}`;
      if (parts[2] === "ws" && parts.length === 3) {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
          return json({ error: "expected websocket" }, 426);
        }
        return forward(
          env.CHAT_ROOMS,
          room,
          request,
          auth.userId,
          "/ws",
          `?chatId=${parts[1]}${deviceParam(url)}`
        );
      }
      const routes: Record<string, string[]> = {
        checkpoint: ["GET", "POST"],
        // Pull/push over plain HTTPS (the airplane-wifi transport): GET
        // /rows?after= collapses connect→hello→state→rowsReq→backfill into
        // one round trip; POST /rows is the batchId-deduped push twin.
        rows: ["GET", "POST"],
        tail: ["GET", "PUT"],
        diff: ["GET", "PUT"],
        stats: ["GET"],
        reset: ["POST"]
      };
      if (parts.length === 3 && routes[parts[2]]?.includes(request.method)) {
        // Query carries through (`seqCovered` on POST /checkpoint), as do
        // headers (`x-chat2-frontier`, `range`).
        return forward(env.CHAT_ROOMS, room, request, auth.userId, `/${parts[2]}`, url.search);
      }
      return json({ error: "not found" }, 404);
    }

    // ── workspace rooms (ARCHITECTURE §2.2/§6.1): same SessionRoom DO class;
    //    the caller's WorkOS org claim (`org_id`) must equal the URL's orgId,
    //    and the room itself is derived from the caller's OWN user id — the
    //    workspace doc (spaces, chats index, devices) is per-user; teammates
    //    in the same org can never address each other's rooms. ──────────────
    if (parts[0] === "workspace" && parts[1] && ID_RE.test(parts[1])) {
      const orgId = parts[1];
      if (auth.orgId !== orgId) return json({ error: "forbidden" }, 403);
      // `ws4` = the 2026-08-04 incident break: the ws3 instance's storage was
      // left with causally-broken update rows by the abort-thrash loop (acks
      // outran the debounced flush) and could not be trusted again even after
      // /reset-log; a name bump allocates a virgin DO. (`ws3` was the per-user
      // privacy break, `ws2` the spaces overhaul.) Legacy rooms are orphaned
      // (hibernated, ~zero cost). URL path stays `/workspace/:orgId/*`; the
      // name is worker-internal — clients echo their own roomId strings.
      const room = `ws4/${orgId}/${auth.userId}`;
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
          return json({ error: "expected websocket" }, 426);
        }
        return forward(
          env.SESSION_ROOMS,
          room,
          request,
          auth.userId,
          "/ws",
          `?chatId=${encodeURIComponent(room)}${deviceParam(url)}`,
          "workspace"
        );
      }
      if (parts[2] === "tail" && request.method === "GET") {
        return forward(env.SESSION_ROOMS, room, request, auth.userId, "/tail", "", "workspace");
      }
      // Observability: log/snapshot sizes for the per-user workspace room, so a
      // human can see whether the compaction budget is holding (org-membership
      // was already checked above; the DO bypasses the owner gate for
      // workspace kind).
      if (parts[2] === "stats" && request.method === "GET") {
        return forward(env.SESSION_ROOMS, room, request, auth.userId, "/stats", "", "workspace");
      }
      // Raw doc snapshot: the repair/reseed read (2026-08-04: a device stranded
      // behind the shallow-locked rebuild converges by replacing its local
      // workspace doc with this — see the incident repair recipe).
      if (parts[2] === "snapshot" && request.method === "GET") {
        return forward(env.SESSION_ROOMS, room, request, auth.userId, "/snapshot", "", "workspace");
      }
      // Operator wedge-break: clear a workspace room whose update log grew big
      // enough to CPU-reset the DO on every cold start (org-membership already
      // checked; state re-uploads from each device's local doc on rejoin).
      if (parts[2] === "reset-log" && request.method === "POST") {
        return forward(env.SESSION_ROOMS, room, request, auth.userId, "/reset-log", "", "workspace");
      }
      // Merge-safe repair write (the chat rooms' /append, for the workspace
      // doc): lets an operator seed a reset room with ONE compact
      // locally-exported history blob instead of waiting for every device to
      // re-upload its whole doc — the N-way redundant re-seed is what kept
      // ballooning the update log after the 2026-08-05 wedge breaks.
      if (parts[2] === "append" && request.method === "POST") {
        return forward(env.SESSION_ROOMS, room, request, auth.userId, "/append", "", "workspace");
      }
    }

    // ── registry rooms (docs/registry-sync.md): the row-table replacement for
    //    the Loro workspace doc. Same trust shape as /workspace: org claim
    //    must match the URL, room derived from the caller's OWN user id, DO
    //    trusts the stamped header. `reg1` = first registry generation. ─────
    if (parts[0] === "registry" && parts[1] && ID_RE.test(parts[1])) {
      const orgId = parts[1];
      if (auth.orgId !== orgId) return json({ error: "forbidden" }, 403);
      const room = `reg1/${orgId}/${auth.userId}`;
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
          return json({ error: "expected websocket" }, 426);
        }
        return forward(
          env.REGISTRY_ROOMS,
          room,
          request,
          auth.userId,
          "/ws",
          `?${deviceParam(url).replace(/^&/, "")}`
        );
      }
      if (parts[2] === "stats" && request.method === "GET") {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/stats", "");
      }
      // Pull over plain HTTPS: `?since=` returns the same delta the WS
      // hello would (full table without it — the original repair read).
      // One round trip on any network that passes HTTPS at all, where the
      // WS upgrade needs 4 and a cooperative middlebox.
      if (parts[2] === "rows" && request.method === "GET") {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/rows", url.search);
      }
      // Push over plain HTTPS — the WS push's fallback twin (LWW clocks
      // make replays no-ops, so at-least-once delivery is safe).
      if (parts[2] === "push" && request.method === "POST") {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/push", url.search);
      }
      // Operator wipe. Unlike the CRDT rooms this needs no recipe: clients
      // detect the seq regression on their next hello and re-seed the table
      // from local rows with original clocks, automatically.
      if (parts[2] === "reset" && request.method === "POST") {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/reset", "");
      }
    }

    // ── vault control plane (RFC 0001 §6): per-(org, user) like the
    //    registry — org claim must match, room derived from the caller's OWN
    //    user id. The DO validates every sub-path and authenticates every
    //    write cryptographically; the bearer only proves account identity.
    //    `vault1` = first control-plane generation. ────────────────────────
    if (parts[0] === "vault" && parts[1] && ID_RE.test(parts[1])) {
      const orgId = parts[1];
      if (auth.orgId !== orgId) return json({ error: "forbidden" }, 403);
      const room = `vault1/${orgId}/${auth.userId}`;
      const sub = parts.slice(2);
      if (sub.length > 3 || !sub.every((segment) => /^[A-Za-z0-9]{1,64}$/.test(segment))) {
        return json({ error: "not found" }, 404);
      }
      return forward(env.VAULT_ROOMS, room, request, auth.userId, `/${sub.join("/")}`, url.search);
    }

    // ── device rooms ────────────────────────────────────────────────────────
    if (parts[0] === "device" && parts[1] && ID_RE.test(parts[1])) {
      const deviceId = parts[1];
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
          return json({ error: "expected websocket" }, 426);
        }
        const role = url.searchParams.get("role") === "host" ? "host" : "client";
        const connId = url.searchParams.get("connId") ?? crypto.randomUUID();
        // `d2/` — same staging→prod identity break as `s2/` above.
        return forward(
          env.DEVICE_ROOMS,
          `d2/${deviceId}`,
          request,
          auth.userId,
          "/ws",
          `?role=${role}&connId=${encodeURIComponent(connId)}`
        );
      }
      if (parts[2] === "sidecar" && parts[3] && /^[a-z0-9-]{1,64}$/.test(parts[3])) {
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth.userId, `/sidecar/${parts[3]}`, "");
      }
      if (parts[2] === "status") {
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth.userId, "/status", "");
      }
      // Durable command nudge (§7): "chat X has pending commands — open its
      // doc". Delivered live if the host is connected, else queued in the DO
      // and replayed on the host's next join.
      if (parts[2] === "nudge" && request.method === "POST") {
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth.userId, "/nudge", "");
      }
    }

    // ── R2 tool-output sidecar (docs/chat2-sync.md A2): full tool outputs
    //    and diffs live here, keyed `{chatId}/{partId}[.diff]`; the doc keeps
    //    only a one-line summary + this key. Straight R2, no DO involvement —
    //    the doc stays thin whether or not these uploads land. Per-user
    //    prefix = owner auth. ─────────────────────────────────────────────
    if (parts[0] === "blob" && parts.length === 3 && ID_RE.test(parts[1])) {
      // Percent-decode the part segment before validating: PART_RE allows
      // `#` (`m1#c1`-style harness ids), which HTTP clients cannot send raw
      // (fragment delimiter) — the host percent-encodes it. Decode-then-
      // validate keeps traversal shut: `%2F` decodes to `/`, fails PART_RE.
      const partId = safeDecode(parts[2]);
      if (partId === undefined || !PART_RE.test(partId)) {
        return json({ error: "bad part id" }, 400);
      }
      const key = `blob/${auth.userId}/${parts[1]}/${partId}`;
      if (request.method === "PUT") {
        const body = await request.arrayBuffer();
        // Outputs are 4KiB-capped at the harness boundary; diffs can run
        // larger but a sidecar entry is one tool result, never a dump.
        if (body.byteLength > MAX_TOOL_BLOB_BYTES) return json({ error: "too_large" }, 413);
        await env.BLOBS.put(key, body, {
          httpMetadata: {
            contentType: request.headers.get("content-type") ?? "text/plain; charset=utf-8"
          }
        });
        return json({ ok: true, bytes: body.byteLength });
      }
      if (request.method === "GET" || request.method === "HEAD") {
        const object =
          request.method === "GET" ? await env.BLOBS.get(key) : await env.BLOBS.head(key);
        if (!object) return json({ error: "not_found" }, 404);
        const headers = new Headers();
        object.writeHttpMetadata(headers);
        headers.set("etag", object.httpEtag);
        // Re-resolved tool parts overwrite their key, so short-lived caching only.
        headers.set("cache-control", "private, max-age=300");
        const body =
          request.method === "GET" && "body" in object ? (object as R2ObjectBody).body : null;
        return new Response(body, { headers });
      }
    }

    // ── retired attachment mirror (clients ≤0.1.62): acknowledge and discard
    //    so old outboxes drain once instead of retrying the PUT forever ──────
    if (parts[0] === "attachments" && parts[1] && request.method === "PUT") {
      return json({ ok: true });
    }

    return json({ error: "not_found" }, 404);
  }
} satisfies ExportedHandler<Env>;
