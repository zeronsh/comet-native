import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import fixture from "../../../crates/crypto/tests/fixtures/vault.json";
import { AUTH_ORG_HEADER, AUTH_USER_HEADER, ENCRYPTED_ROOM_HEADER } from "../../src/env";
import { decodeBase64 } from "../../src/vault-records";
import { decodeFrame, encodeFrame, FRAME } from "../../src/chat-frames";
import { encodeDeviceFrame } from "../../src/device-room";

function profile() {
  const org = `org-${crypto.randomUUID()}`;
  const user = "test-user";
  const headers = { [AUTH_USER_HEADER]: user, [AUTH_ORG_HEADER]: org };
  const vault = env.VAULT_ROOMS.get(env.VAULT_ROOMS.idFromName(`vault1/${org}/${user}`));
  return { org, headers, vault };
}

async function activate(vault: DurableObjectStub, headers: Record<string, string>) {
  const response = await vault.fetch("https://vault/membership", {
    method: "POST", headers, body: decodeBase64(fixture.membership[0]!)!
  });
  expect(response.status).toBe(200);
}

function nextMessage(socket: WebSocket): Promise<string | ArrayBuffer> {
  return new Promise((resolve, reject) => {
    socket.addEventListener("message", (event) => resolve(event.data), { once: true });
    socket.addEventListener("close", () => reject(new Error("closed before reply")), { once: true });
  });
}

async function connect(stub: DurableObjectStub, headers: Record<string, string>, path = "/ws") {
  const response = await stub.fetch(`https://room${path}`, { headers: { ...headers, Upgrade: "websocket" } });
  expect(response.status).toBe(101);
  const socket = response.webSocket!;
  socket.binaryType = "arraybuffer";
  socket.accept();
  return socket;
}

describe("vault fences on actual Durable Objects", () => {
  it("fences an already-open plaintext chat socket after vault creation", async () => {
    const { org, headers, vault } = profile();
    const room = env.TEST_CHAT.get(env.TEST_CHAT.idFromName(org));
    const socket = await connect(room, headers);
    const hello = nextMessage(socket);
    socket.send(encodeFrame(FRAME.hello, { device: "device", cursor: 0 }));
    expect(decodeFrame(new Uint8Array(await hello as ArrayBuffer))?.type).toBe(FRAME.state);
    await activate(vault, headers);
    const rejection = nextMessage(socket);
    socket.send(encodeFrame(FRAME.push, { batchId: "plain" }, new TextEncoder().encode("plaintext canary")));
    expect(decodeFrame(new Uint8Array(await rejection as ArrayBuffer))?.header.code).toBe("encrypted_profile");
    const stats = await room.fetch("https://room/stats", { headers });
    expect((await stats.json<{ rowCount: number }>()).rowCount).toBe(0);
    socket.close();
  });

  it("fences an already-open plaintext registry socket after vault creation", async () => {
    const { org, headers, vault } = profile();
    const room = env.TEST_REGISTRY.get(env.TEST_REGISTRY.idFromName(org));
    const socket = await connect(room, headers);
    const hello = nextMessage(socket);
    socket.send(JSON.stringify({ t: "hello", device: "device", cursor: 0 }));
    expect(JSON.parse(await hello as string).t).toBe("state");
    await activate(vault, headers);
    const rejection = nextMessage(socket);
    socket.send(JSON.stringify({ t: "push", batch: "plain", ops: [] }));
    expect(JSON.parse(await rejection as string).code).toBe("encrypted_profile");
    const stats = await room.fetch("https://room/stats", { headers });
    expect((await stats.json<{ rowCount: number }>()).rowCount).toBe(0);
    socket.close();
  });

  it("preserves sealed bytes across WebSocket send and HTTPS retry/pull", async () => {
    const { org, headers } = profile();
    const room = env.TEST_CHAT.get(env.TEST_CHAT.idFromName(org));
    const socket = await connect(room, { ...headers, [ENCRYPTED_ROOM_HEADER]: "1" });
    const hello = nextMessage(socket);
    socket.send(encodeFrame(FRAME.hello, { device: "device", cursor: 0 }));
    await hello;
    const bytes = decodeBase64(fixture.chatRecord)!;
    const ack = nextMessage(socket);
    socket.send(encodeFrame(FRAME.push, { batchId: "sealed" }, bytes));
    expect(decodeFrame(new Uint8Array(await ack as ArrayBuffer))?.header.seq).toBe(1);
    const retry = await room.fetch("https://room/rows?device=device&batchId=sealed", {
      method: "POST", headers, body: bytes
    });
    expect(await retry.json()).toMatchObject({ seq: 1, dup: true });
    const pull = await room.fetch("https://room/rows?after=0", { headers });
    const body = new Uint8Array(await pull.arrayBuffer());
    const view = new DataView(body.buffer);
    const rows: Uint8Array[] = [];
    for (let offset = 0; offset < body.length;) {
      const length = view.getUint32(offset, true);
      const frame = decodeFrame(body.subarray(offset + 4, offset + 4 + length))!;
      if (frame.type === FRAME.row) rows.push(frame.payload);
      offset += 4 + length;
    }
    expect(rows).toHaveLength(1);
    expect(Array.from(rows[0]!)).toEqual(Array.from(bytes));
    socket.close();
  });

  it("rejects plaintext over HTTPS in an encrypted registry generation", async () => {
    const { org, headers } = profile();
    const room = env.TEST_REGISTRY.get(env.TEST_REGISTRY.idFromName(org));
    const response = await room.fetch("https://room/push", {
      method: "POST", headers: { ...headers, [ENCRYPTED_ROOM_HEADER]: "1" },
      body: JSON.stringify({ batch: "plain", ops: [{ kind: "chats", id: "chat", op: "upsert", hlc: "0000000000001-000000-device", set: { title: "plaintext canary" } }] })
    });
    expect(response.status).toBe(400);
    expect((await response.json<{ error: string }>()).error).toBe("plaintext_rejected");
    const stats = await room.fetch("https://room/stats", { headers });
    expect((await stats.json<{ rowCount: number }>()).rowCount).toBe(0);
  });

  it("rejects plaintext over HTTPS in an encrypted chat generation", async () => {
    const { org, headers } = profile();
    const room = env.TEST_CHAT.get(env.TEST_CHAT.idFromName(org));
    const socket = await connect(room, { ...headers, [ENCRYPTED_ROOM_HEADER]: "1" });
    const response = await room.fetch("https://room/rows?device=device&batchId=plain", {
      method: "POST", headers, body: "plaintext canary"
    });
    expect(response.status).toBe(400);
    expect((await response.json<{ error: string }>()).error).toBe("plaintext_rejected");
    socket.close();
  });

  it("refuses legacy device sidecars and RPC once encryption is required", async () => {
    const { org, headers, vault } = profile();
    const room = env.TEST_DEVICE.get(env.TEST_DEVICE.idFromName(org));
    const socket = await connect(room, headers, "/ws?role=host");
    await activate(vault, headers);
    const response = await room.fetch("https://room/sidecar/repos", {
      method: "POST", headers, body: JSON.stringify({ path: "plaintext canary" })
    });
    expect(response.status).toBe(409);
    const stored = await room.fetch("https://room/sidecar/repos", { headers });
    expect(stored.status).toBe(404);
    const closed = new Promise<number>((resolve) => socket.addEventListener("close", (event) => resolve(event.code), { once: true }));
    socket.send(encodeDeviceFrame({ s: "rpc", k: "rpc", to: "peer" }, new TextEncoder().encode("plaintext canary")));
    expect(await closed).toBe(4403);
  });

  it("serializes competing genesis publications without a storage exception", async () => {
    const { headers, vault } = profile();
    const publish = () => vault.fetch("https://vault/membership", {
      method: "POST", headers, body: decodeBase64(fixture.membership[0]!)!
    });
    const results = await Promise.all([publish(), publish()]);
    expect(results.map((r) => r.status).sort()).toEqual([200, 409]);
    const page = await vault.fetch("https://vault/membership?after=-1", { headers });
    expect((await page.json<{ records: string[] }>()).records).toHaveLength(1);
  });
});
