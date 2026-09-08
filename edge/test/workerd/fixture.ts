import { DurableObject } from "cloudflare:workers";
export { ChatRoom } from "../../src/chat-room";
export { RegistryRoom } from "../../src/registry-room";
export { VaultRoom } from "../../src/vault-room";
export { DeviceRoom } from "../../src/device-room";

/** Bare SQLite-backed DO; tests reach its real `ctx.storage.sql` via
 * `runInDurableObject` (the cloudflare-os TEST_OVERSEER pattern). */
export class TestLogRoom extends DurableObject {}

export default {
  fetch(): Response {
    return new Response("test fixture", { status: 404 });
  }
};
