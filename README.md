# comet-native

A native rewrite of [comet](https://github.com/wingleeio/comet) — a multi-device controller for
coding agents (Claude Code / Codex) — in Rust with a [gpui](https://gpui.rs) UI.

Each device runs an **engine** that executes agents and syncs state as **Loro CRDT docs** through
**Cloudflare Durable Objects** (per-chat session rooms + per-device relay rooms). The gpui app is
a thin viewport over its local engine; a UI on one device can drive an agent on another through
the device-room relay. One binary: headed by default, `comet headless` for VPS/remote devices.

```
gpui UI ─ in-proc/localhost RPC ─ engine A ══ DeviceRoom DO relay ══ engine B
                    │            (edge Worker: auth, rooms, R2)        │
                    └── Loro sync ── SessionRoom DO (per chat) ────────┘
```

No Orbit, no Postgres, no Electron, no WebRTC — see [ARCHITECTURE.md](ARCHITECTURE.md).

## Layout

```
crates/
  proto/     wire types (AgentEvent, ToolCall, entities)
  doc/       session/workspace doc schemas, mirror layer, command ledger
  sync/      loro room client + local snapshot store
  harness/   Claude Code (stream-json) / Codex (app-server) adapters
  engine/    the headless backend
  rpc/       UiRpc/ControlRpc transports + device-room virtual sockets
  ui/        the gpui app
apps/comet/  the binary
edge/        TypeScript Cloudflare Worker + Durable Objects
docs/        architecture + research notes
```

## Build

```bash
cargo build --workspace       # needs the gpui Linux deps (see docs/research/gpui.md)
cargo test  --workspace
cd edge && npm install && npm run dev   # wrangler dev on :26640
```
