/**
 * End-to-end smoke test against a running `wrangler dev` instance
 * (AUTH_MODE=dev). Exercises the full design surface:
 *   1. two Loro peers join a session room and converge through the DO
 *   2. streamed text appends propagate live
 *   3. GET /tail returns the materialized L2 tail
 *   4. POST/GET /diff round-trips the sidecar
 *   5. ephemeral (%EPH) presence relays between peers
 *   6. device room relays client↔host frames and serves sidecar slots
 *   7. R2 attachments: PUT (hash verified) then GET
 *   8. workspace room (`ws/{orgId}`): two org members converge; wrong org 403
 *   9. absorbed /auth routes: 501 without WORKOS_API_KEY; cli callback page
 *
 * Usage: node scripts/smoke.mjs [baseUrl]   (default http://127.0.0.1:27640)
 */
import { LoroDoc } from "loro-crdt";
import { LoroWebsocketClient } from "loro-websocket";
import { LoroAdaptor, LoroEphemeralAdaptor } from "loro-adaptors/loro";
import { createHash, randomUUID } from "node:crypto";

const base = process.argv[2] ?? "http://127.0.0.1:27640";
const wsBase = base.replace(/^http/, "ws");
const token = "smoke-user";
const chatId = `smoke-${randomUUID().slice(0, 8)}`;
const deviceId = `smokedev-${randomUUID().slice(0, 8)}`;
const orgId = `org-smoke-${randomUUID().slice(0, 8)}`;

const fail = (msg) => {
  console.error(`✗ ${msg}`);
  process.exit(1);
};
const ok = (msg) => console.log(`✓ ${msg}`);
const until = async (fn, what, ms = 8000) => {
  const start = Date.now();
  while (Date.now() - start < ms) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  fail(`timeout waiting for ${what}`);
};

// ── health ────────────────────────────────────────────────────────────────
{
  const res = await fetch(`${base}/health`);
  const body = await res.json();
  if (!body.ok) fail("health");
  if (body.auth !== "dev") fail(`expected dev auth mode, got ${body.auth} — run wrangler dev with --var AUTH_MODE:dev`);
  ok("health (dev auth)");
}

// ── session room: two peers converge ─────────────────────────────────────
const sessionUrl = `${wsBase}/session/${chatId}/ws?token=${token}`;

const clientA = new LoroWebsocketClient({ url: sessionUrl });
await clientA.waitConnected();
const adaptorA = new LoroAdaptor();
await clientA.join({ roomId: chatId, crdtAdaptor: adaptorA });
const docA = adaptorA.getDoc();
docA.getMap("meta").set("chatId", chatId);
docA.getMap("meta").set("schemaVersion", 1);
const messagesA = docA.getList("messages");
const m1 = messagesA.insertContainer(0, new (await import("loro-crdt")).LoroMap());
m1.set("id", "m1");
m1.set("role", "user");
m1.set("createdAt", Date.now());
m1.set("deviceId", "peer-a");
docA.commit();
ok("peer A joined + wrote");

const clientB = new LoroWebsocketClient({ url: `${wsBase}/session/${chatId}/ws?token=${token}` });
await clientB.waitConnected();
const adaptorB = new LoroAdaptor();
await clientB.join({ roomId: chatId, crdtAdaptor: adaptorB });
const docB = adaptorB.getDoc();
await until(() => docB.getList("messages").length > 0, "peer B backfill");
ok("peer B backfilled through DO");

// live propagation A→B
const t0 = docA.getList("messages").get(0);
docA.getMap("meta").set("title", "smoke");
docA.commit();
await until(() => docB.getMap("meta").get("title") === "smoke", "live A→B");
ok("live update A→B");

// live propagation B→A
docB.getMap("meta").set("fromB", true);
docB.commit();
await until(() => docA.getMap("meta").get("fromB") === true, "live B→A");
ok("live update B→A");
void t0;

// ── wrong user rejected ───────────────────────────────────────────────────
{
  const res = await fetch(`${base}/tail/${chatId}?token=intruder`);
  if (res.status !== 403) fail(`intruder tail expected 403, got ${res.status}`);
  ok("ownership enforced (intruder 403)");
}

// ── tail ──────────────────────────────────────────────────────────────────
await new Promise((r) => setTimeout(r, 100));
{
  const res = await fetch(`${base}/tail/${chatId}?token=${token}`);
  if (res.status !== 200) fail(`tail status ${res.status}`);
  const tail = await res.json();
  if (tail.chatId !== chatId) fail(`tail chatId ${tail.chatId}`);
  if (tail.totalMessages < 1) fail("tail totalMessages");
  ok(`tail (${tail.totalMessages} messages)`);
}

// ── diff sidecar ──────────────────────────────────────────────────────────
{
  const diff = { chatId, deviceId: "peer-a", checkoutPath: "/tmp/x", patch: "diff --git a b", files: [], additions: 1, deletions: 0, truncated: false, publishedAt: Date.now() };
  const post = await fetch(`${base}/diff/${chatId}?token=${token}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(diff)
  });
  if (post.status !== 200) fail(`diff post ${post.status}`);
  const get = await fetch(`${base}/diff/${chatId}?token=${token}`);
  const body = await get.json();
  if (body.patch !== diff.patch) fail("diff round-trip");
  ok("diff sidecar round-trip");
}

// ── ephemeral presence ────────────────────────────────────────────────────
{
  const ephA = new LoroEphemeralAdaptor();
  await clientA.join({ roomId: chatId, crdtAdaptor: ephA });
  const ephB = new LoroEphemeralAdaptor();
  await clientB.join({ roomId: chatId, crdtAdaptor: ephB });
  ephA.getStore().set("presence:peer-a", { status: "busy" });
  await until(
    () => ephB.getStore().get("presence:peer-a")?.status === "busy",
    "ephemeral A→B"
  );
  ok("ephemeral presence relay");
}

// ── workspace room: org-membership authz, two members converge ────────────
{
  // Dev-mode org claim: token `userId@orgId`.
  const roomId = `ws/${orgId}`;
  const memberA = new LoroWebsocketClient({
    url: `${wsBase}/workspace/${orgId}/ws?token=alice@${orgId}`
  });
  await memberA.waitConnected();
  const wsAdaptorA = new LoroAdaptor();
  await memberA.join({ roomId, crdtAdaptor: wsAdaptorA });
  const wsDocA = wsAdaptorA.getDoc();
  wsDocA.getMap("meta").set("chatId", roomId);
  wsDocA.getMap("chats").set("chat-1", { title: "hello" });
  wsDocA.commit();

  // A DIFFERENT user of the same org joins the same room — membership, not
  // per-chat ownership (which would have rejected the second user).
  const memberB = new LoroWebsocketClient({
    url: `${wsBase}/workspace/${orgId}/ws?token=bob@${orgId}`
  });
  await memberB.waitConnected();
  const wsAdaptorB = new LoroAdaptor();
  await memberB.join({ roomId, crdtAdaptor: wsAdaptorB });
  await until(
    () => wsAdaptorB.getDoc().getMap("chats").get("chat-1") !== undefined,
    "workspace member B backfill"
  );
  ok("workspace room: two org members converge");

  // Wrong org claim rejected at the Worker.
  const wrongOrg = await fetch(`${base}/workspace/${orgId}/tail?token=mallory@org-other`);
  if (wrongOrg.status !== 403) fail(`wrong-org tail expected 403, got ${wrongOrg.status}`);
  // No org claim at all is rejected too.
  const noOrg = await fetch(`${base}/workspace/${orgId}/tail?token=${token}`);
  if (noOrg.status !== 403) fail(`no-org tail expected 403, got ${noOrg.status}`);
  // A member can read the workspace tail (empty messages — shape only).
  const memberTail = await fetch(`${base}/workspace/${orgId}/tail?token=alice@${orgId}`);
  if (memberTail.status !== 200) fail(`member workspace tail ${memberTail.status}`);
  ok("workspace room: org membership enforced (403 for outsiders)");

  memberA.close();
  memberB.close();
}

// ── device room ───────────────────────────────────────────────────────────
{
  const { encodeDeviceFrame, decodeDeviceFrame } = await import("./device-frame.mjs");
  const host = new WebSocket(`${wsBase}/device/${deviceId}/ws?token=${token}&role=host`);
  host.binaryType = "arraybuffer";
  await new Promise((resolve, reject) => {
    host.onopen = resolve;
    host.onerror = reject;
  });
  const hostFrames = [];
  host.onmessage = (e) => {
    if (typeof e.data === "string") return;
    const frame = decodeDeviceFrame(new Uint8Array(e.data));
    hostFrames.push(frame);
    // echo rpc payloads back to the sender
    if (frame.header.k === "rpc" && frame.header.from) {
      host.send(
        encodeDeviceFrame(
          { s: frame.header.s, k: "rpc", to: frame.header.from },
          frame.payload
        )
      );
    }
  };

  const connId = "conn-1";
  const client = new WebSocket(`${wsBase}/device/${deviceId}/ws?token=${token}&role=client&connId=${connId}`);
  client.binaryType = "arraybuffer";
  await new Promise((resolve, reject) => {
    client.onopen = resolve;
    client.onerror = reject;
  });
  const clientFrames = [];
  client.onmessage = (e) => {
    if (typeof e.data === "string") return;
    clientFrames.push(decodeDeviceFrame(new Uint8Array(e.data)));
  };
  client.send(encodeDeviceFrame({ s: "rpc-1", k: "rpc" }, new TextEncoder().encode("hello-host")));
  await until(() => clientFrames.length > 0, "device rpc echo");
  const echoed = new TextDecoder().decode(clientFrames[0].payload);
  if (echoed !== "hello-host") fail(`device echo got ${echoed}`);
  ok("device room rpc echo (client→host→client)");

  // intruder cannot join the device room
  const evil = new WebSocket(`${wsBase}/device/${deviceId}/ws?token=evil&role=client`);
  const evilResult = await new Promise((resolve) => {
    evil.onopen = () => resolve("open");
    evil.onerror = () => resolve("error");
    setTimeout(() => resolve("timeout"), 3000);
  });
  if (evilResult === "open") fail("intruder joined device room");
  ok("device room ownership enforced");

  // sidecar slot
  const post = await fetch(`${base}/device/${deviceId}/sidecar/repos?token=${token}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ repos: [{ path: "/x", name: "x" }] })
  });
  if (post.status !== 200) fail(`sidecar post ${post.status}`);
  const got = await (await fetch(`${base}/device/${deviceId}/sidecar/repos?token=${token}`)).json();
  if (got.repos?.[0]?.name !== "x") fail("sidecar round-trip");
  ok("device sidecar slot round-trip");

  // nudge: live delivery to the connected host
  const nudge = await fetch(`${base}/device/${deviceId}/nudge?token=${token}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ chatId: "chat-live" })
  });
  if ((await nudge.json()).delivered !== true) fail("live nudge not delivered");
  await until(
    () => hostFrames.some((f) => f.header.k === "nudge" && new TextDecoder().decode(f.payload).includes("chat-live")),
    "live nudge frame"
  );
  ok("nudge delivered live to connected host");

  host.close();
  client.close();

  // nudge: queued while host offline, replayed on rejoin
  await new Promise((r) => setTimeout(r, 200)); // let the close land
  const queued = await fetch(`${base}/device/${deviceId}/nudge?token=${token}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ chatId: "chat-cold" })
  });
  if ((await queued.json()).queued !== true) fail("offline nudge not queued");
  const host2 = new WebSocket(`${wsBase}/device/${deviceId}/ws?token=${token}&role=host`);
  host2.binaryType = "arraybuffer";
  const replayed = [];
  host2.onmessage = (e) => {
    if (typeof e.data === "string") return;
    replayed.push(decodeDeviceFrame(new Uint8Array(e.data)));
  };
  await until(
    () => replayed.some((f) => f.header.k === "nudge" && new TextDecoder().decode(f.payload).includes("chat-cold")),
    "queued nudge replay on host join"
  );
  ok("nudge queued offline and replayed on host join");
  host2.close();
}

// ── attachments ───────────────────────────────────────────────────────────
{
  const bytes = new TextEncoder().encode(`attachment-${chatId}`);
  const hash = createHash("sha256").update(bytes).digest("hex");
  const put = await fetch(`${base}/attachments/${hash}?token=${token}`, {
    method: "PUT",
    headers: { "content-type": "image/png" },
    body: bytes
  });
  if (put.status !== 200) fail(`attachment put ${put.status}: ${await put.text()}`);
  const get = await fetch(`${base}/attachments/${hash}?token=${token}`);
  if (get.status !== 200) fail(`attachment get ${get.status}`);
  const round = new Uint8Array(await get.arrayBuffer());
  if (new TextDecoder().decode(round) !== `attachment-${chatId}`) fail("attachment bytes");
  const bad = await fetch(`${base}/attachments/${"0".repeat(64)}?token=${token}`, {
    method: "PUT",
    body: bytes
  });
  if (bad.status !== 400) fail(`hash mismatch expected 400, got ${bad.status}`);
  ok("R2 attachments (hash-verified put/get)");
}

// ── absorbed auth routes ──────────────────────────────────────────────────
{
  // Dev instances have no WORKOS_API_KEY: secret-bearing routes answer 501
  // (matching the old apps/server behavior when WorkOS is unconfigured).
  const exchange = await fetch(`${base}/auth/exchange`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ code: "test" })
  });
  if (exchange.status !== 501) fail(`auth exchange expected 501 in dev, got ${exchange.status}`);
  const refresh = await fetch(`${base}/auth/refresh`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ refreshToken: "test" })
  });
  if (refresh.status !== 501) fail(`auth refresh expected 501 in dev, got ${refresh.status}`);
  ok("auth exchange/refresh answer 501 without WORKOS_API_KEY");

  // The headless callback needs no WorkOS config: it just renders state.code.
  const cb = await fetch(`${base}/auth/cli/callback?code=abc123&state=xyz789`);
  if (cb.status !== 200) fail(`cli callback ${cb.status}`);
  const page = await cb.text();
  if (!page.includes("xyz789.abc123")) fail("cli callback paste code missing");
  const cbBad = await fetch(`${base}/auth/cli/callback`);
  if (cbBad.status !== 400) fail(`cli callback without code expected 400, got ${cbBad.status}`);
  ok("auth cli callback renders paste code");
}

// ── reconnect: new client with existing state catches up incrementally ───
{
  const clientC = new LoroWebsocketClient({ url: `${wsBase}/session/${chatId}/ws?token=${token}` });
  await clientC.waitConnected();
  const preSeeded = new LoroDoc();
  preSeeded.import(docA.export({ mode: "snapshot" }));
  const adaptorC = new LoroAdaptor(preSeeded);
  await clientC.join({ roomId: chatId, crdtAdaptor: adaptorC });
  docA.getMap("meta").set("afterC", 1);
  docA.commit();
  await until(() => adaptorC.getDoc().getMap("meta").get("afterC") === 1, "peer C incremental");
  ok("version-vector incremental join");
  clientC.close();
}

clientA.close();
clientB.close();
console.log("\nALL SMOKE TESTS PASSED");
process.exit(0);
