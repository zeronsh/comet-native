import { defineConfig } from "vitest/config";
import { cloudflareTest } from "@cloudflare/vitest-pool-workers";

// Runtime-real test tier: runs inside actual workerd via
// @cloudflare/vitest-pool-workers, against a real SQLite-backed Durable
// Object, so platform limits like the ~2MB SQLITE_TOOBIG row cap (the
// 2026-08-05 whale sync freeze) are the runtime's own, not FakeSql constants.
// `npm run test:workerd`.
//
// loro-wasm does NOT work in this tier: the pool's test runner evaluates
// modules where wasm codegen is disallowed, while a real worker compiles the
// base64-inlined module at startup (deployed edge and `wrangler dev` are
// fine). loro-on-workerd coverage lives in the wrangler-dev scripts
// (scripts/whale-check.mjs, scripts/fold-check.mjs).
export default defineConfig({
  plugins: [
    cloudflareTest({
      main: "./test/workerd/fixture.ts",
      miniflare: {
        compatibilityDate: "2026-07-01",
        bindings: { AUTH_MODE: "workos" },
        durableObjects: {
          TEST_LOG: { className: "TestLogRoom", useSQLite: true },
          TEST_CHAT: { className: "ChatRoom", useSQLite: true },
          TEST_REGISTRY: { className: "RegistryRoom", useSQLite: true },
          TEST_DEVICE: { className: "DeviceRoom", useSQLite: true },
          VAULT_ROOMS: { className: "VaultRoom", useSQLite: true }
        }
      }
    })
  ],
  resolve: {
    // Mirror wrangler.jsonc: workerd cannot fetch loro's WASM by URL; the
    // base64 entry inlines it.
    alias: { "loro-crdt": "loro-crdt/base64" }
  },
  test: {
    include: ["test/workerd/**/*.test.ts"]
  }
});
