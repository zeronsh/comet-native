# Android Protocol Contract

Source of truth: iOS Swift clients + Rust crates + edge routes. No new endpoint invented here.

## Auth

Source: `apps/ios/Zeron/Auth/AuthClient.swift`, `edge/src/auth-routes.ts`

| Route | Method | Request JSON | Response JSON | Auth header |
|-------|--------|--------------|---------------|-------------|
| `auth/exchange` | POST | `{ code: string }` | `{ user: { id, email?, firstName?, lastName? }, accessToken, refreshToken }` | none |
| `auth/refresh` | POST | `{ refreshToken: string, organizationId?: string }` | `{ accessToken, refreshToken }` | none |
| `auth/orgs` | GET | — | `{ orgs: [{ id, organizationId, name }] }` | `Bearer <accessToken>` |

Behavior:
- POST bodies are `application/json`. Non-2xx preserves status + body text as `AuthError.http`.
- `refresh` with `organizationId` re-scopes token with `org_id` claim required by workspace room.
- iOS `Keychain` service `sh.zeron.ios`; Android will use EncryptedSharedPreferences / Keystore equivalent.

Dev mode (`AUTH_MODE=dev` edge):
- Bearer is literally `userId` or `userId@orgId` (engine `AuthClient`). No exchange; user+org fields injected via debug config.
- Production config must not point to dev endpoint; dev path is test/debug-only.

## Registry (workspace index)

Source: `apps/ios/Zeron/Sync/RegistryClient.swift`, `apps/ios/Zeron/Sync/WorkspaceStore.swift`, `docs/registry-sync.md`, `edge/src/registry-room.ts`, `crates/sync/src/registry.rs`

- WebSocket: `/registry/{orgId}/ws` (also HTTPS fallback `GET /registry/{orgId}/rows?since=cursor` and `POST /registry/{orgId}/push`)
- Frames are JSON text.

Client→server: `hello {t:"hello", cursor: number|null, device}`, `push {t:"push", batch, ops}`, `presence {t:"presence", at: int64 ms}`, `probe {t:"probe"}`.
Server→client: `state {t:"state", seq, full, gcFloor, rows, presence}`, `rows {t:"rows", seq, rows}`, `ack {t:"ack", batch, seq, applied}`, `presence {t:"presence", device, at}`, `probe-ok {t:"probe-ok", seq}`, `error {t:"error", code, message}`.

Row model: `Row { kind, id, seq, deleted, delHlc?, fields: {field: value|null}, clocks }`, Op `{kind, id, op:"upsert"|"update"|"delete", set?, hlc:"{ms}-{counter}-{device}", clocks? }`.
kinds: `devices`, `spaces`, `chats`, `sessions`. iOS projects them into `DeviceRow`, `Space`, `Chat`, `SessionRow` (see WorkspaceStore.swift:310+).

Protocol rules:
- `hello` must be first frame; `cursor=null` asks full state. `hello`/`probe` deadlines: 15s / 10s; silence lease 45s; 15min quiet-room probe; backoff 250ms→16s, stable-reset 30s.
- `state.full=false` means rows are delta `seq > cursor`. Cursor advances only through contiguous `seq`.
- `update` never creates/revives rows; `upsert` revives tombstones iff newer than `delHlc`.
- `presence` beats every 15s, TTL 30s (receipt-time). `ping` text → `pong` proves transport only, not DO health; health judged by protocol frames.

Workspace HTTP fallback (WorkspaceStore `pullDelta`/`pushPendingOverHTTP`):
- `GET /registry/{orgId}/rows?since=<cursor>` → same `state` body.
- `POST` pushes pending batches with same `ack` semantics; replays idempotent via HLC `>` compare.

## Chat2 (session transcript)

Source: `apps/ios/Zeron/Sync/ChatRoomClient.swift`, `apps/ios/Zeron/Sync/SessionStore.swift`, `crates/sync/src/chat_client.rs`, `crates/sync/src/chat_frames.rs`, `docs/chat2-sync.md`

- WebSocket per chat: `/chat2/{chatId}/ws` (binary frames)
- Wire: binary WS `data` frames decoded by `ChatWire` — 1-byte type + JSON header + raw payload. Types: `hello`, `state`, `rowsReq`, `row`, `rowsDone`, `push`, `ack`, `probe`, `probeOk`, `error`, `presence`.
- Hello: `hello {cursor: UInt64, device}`. State reply: `state {seqFloor, headSeq, checkpointSeq, checkpointFrontier, checkpointSize}` (header) + opaque `checkpointFrontier` payload.
- Backfill: client compares `checkpointFrontier` against local Loro frontiers. Contained → `rowsReq {after: max(cursor, checkpointSeq), excludeOwn: resumed}`. Not contained → `GET /chat2/{chatId}/checkpoint` (Range-resumable, `x-chat2-checkpoint-seq` header, seq replacement restarts from 0) then rows after `checkpointSeq`.
- Rows streaming: `rows {after}` → server streams `row {seq, payload}` + terminal `rowsDone`.
- Push: `push {batchId, bytes}` where `bytes` is a Loro update blob (≤ ~1MiB minus header). Client cap `maxPushBytes = 1MiB - 4096`. `ack {batchId, seq}` advances cursor only if `seq == cursor+1`; gap `seq > cursor+1` triggers `rowsReq` backfill (contiguity rule, max 3 repairs then redial).
- Error codes: `too_large`, `empty`, `bad_push` (permanent, retire batch), `quota` (transient, 5s retry head), `hello_first`.
- HTTP fallbacks: `GET /chat2/{chatId}/rows?after=`, `GET /chat2/{chatId}/checkpoint` (Range), `POST /chat2/{chatId}/rows?batchId=` (pullSync path for WS-less networks).
- Liveness: same dead-socket discipline — `ping`/`pong` not sufficient. Deadlines: hello 15s, backfill 120s, probe 10s, quiet 15min.
- Loro rows: session doc containers `entries/parts` + command ledger (see Loro doc).

## Device relay

Source: `apps/ios/Zeron/Sync/DeviceRelayClient.swift`, `WorkspaceStore.swift` relay section, `edge/src/device-room.ts` (if present)

- Binary `uleb128(len)+header+payload` frames, header JSON `{s,k,to,from}`, ndjson `ControlRpc`. Used for `ListFolders`, `Mutate {createSpace}`, `ListHarnesses`, `ListModels`, etc. MVP requires only folder/space creation for parity; keep envelope exact — do not re-encode.

## Loro document containers (wire-relevant names)

Source: `apps/ios/Zeron/Sync/SessionStore.swift`, `apps/ios/Zeron/Sync/WorkspaceStore.swift`, `crates/doc/src/`

- RegistryDoc kinds as above; no CRDT in Kotlin.
- Session doc: `entries` / `parts` with continuation joining, `commands` ledger (client appends `run`/`steer`/`interrupt`/`respondInput` with client-minted message ids). Host writes entries, command outcomes, and tool/diff parts. Viewer writes only ledger commands (and workspace chat fields `archived`/`title`/`lastSeenAt`). See android-loro-api.md for minimal ops.

## Unknowns (do not guess)

- Exact edge HTTP paths for chat2 `rows`/`checkpoint`/`push` — iOS uses `AppConfig.registrySocketURL()` + `rowsRequest(after)` closures; confirm `edge/src/chat-room.ts` route strings before freezing Android `AppConfig` URLs.
- Production edge base URL and WorkOS redirect URL shape for Android deep link (iOS uses code-paste flow; Android plan uses Custom Tab + deep link — scheme/host must match `AndroidManifest` intent filter and be validated with state/nonce).
- Device-id format expected by registry (iOS uses persisted UUID string; confirm server validates shape).
- Whether `registry-room` `probe-ok` carries `seq` (docs say yes, RegistryClient decodes without it) — treat as op-tolerant.
