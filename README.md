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

## Status

M0–M6 landed: local + multi-device chat (doc-queued commands, host execution, CRDT sync
proven live by the e2e smoke), WorkOS or dev auth, terminals, diff pane, repos/worktrees,
agent accounts, Claude + Codex harnesses, Linux packaging. Honest per-feature ledger:
[docs/PARITY.md](docs/PARITY.md); milestone detail: [ARCHITECTURE.md §8](ARCHITECTURE.md).

## Layout

```
crates/
  proto/     wire types (AgentEvent, ToolCall, entities, AuthState)
  doc/       session/workspace doc schemas, mirror layer, command ledger
  sync/      loro room client + local snapshot store
  harness/   Claude Code (stream-json) / Codex (app-server) adapters + mock
  engine/    the headless backend (sessions, doc host, auth, terminals,
             repos/diffs, uploads, agent accounts, device-room host)
  rpc/       UiRpc/ControlRpc transports + device-room virtual sockets
             (examples: e2e_driver, rpc_probe)
  ui/        the gpui app
apps/comet/  the binary (headed by default; `comet headless`)
edge/        TypeScript Cloudflare Worker + Durable Objects
dist/        packaging assets (.desktop, icon, macOS Info.plist template)
scripts/     e2e-smoke.sh, package-linux.sh
docs/        PARITY.md + research notes
```

## Build & test

```bash
cargo build --workspace       # Linux: needs the gpui deps (see docs/research/gpui.md)
cargo test  --workspace
cargo clippy --workspace --all-targets && cargo fmt --all --check
cd edge && npm install && npm run dev   # wrangler dev on :27640 (dev auth: bearer = user@org)
```

**macOS**: `xcode-select --install` (gpui needs the Metal toolchain; full Xcode 15+ if the
shader compile complains) + rustup, then `cargo run -p comet`. Heads-up: this workspace has
only ever been compiled on Linux — the `#[cfg(target_os = "macos")]` paths (Keychain access
in agent accounts) parse but have never been type-checked against the Apple SDK, so the
first macOS build may surface errors there; they're isolated to `crates/engine/src/
agent_accounts.rs` and safe to stub if needed. Window chrome (traffic-light inset,
vibrancy) is untested on real macOS — see dist/README.md for bundling.

## Run

```bash
# Headed (connects to a running daemon on COMET_IPC_PORT, else embeds the engine):
cargo run -p comet

# Headless engine (VPS / second device):
COMET_DATA_DIR=~/.comet-native \
COMET_EDGE_URL=http://localhost:27640 \
COMET_EDGE_TOKEN=alice@org1 \
COMET_ORG_ID=org1 \
COMET_IPC_PORT=27654 \
cargo run -p comet -- headless
```

`COMET_EDGE_TOKEN` is the dev-mode bearer (`user@org`); omit it to run fully offline.
Other knobs: `COMET_HARNESS` (`claude-code` default | `codex` | `mock`) picks the default
harness for chats without a config row; `COMET_WORKOS_CLIENT_ID` enables real WorkOS auth
(dev mode otherwise); `COMET_DEVICE_NAME` overrides the registry hostname.

## Two-device e2e smoke

```bash
scripts/e2e-smoke.sh
```

Starts (or reuses) the edge under `wrangler dev`, boots two headless engines as the same
user on different devices, then drives both IPCs (`crates/rpc/examples/e2e_driver.rs`):
create a chat hosted on device A → queue a run **from device B through the doc command
queue** → the durable nudge wakes A → A executes via the mock harness → the transcript
and session status sync A → edge → B. Prints `PASS`/`FAIL` per step, exits nonzero on
failure, cleans up its processes and temp dirs.

## Packaging

```bash
scripts/package-linux.sh      # tar.gz with binary + .desktop + icon (release, thin LTO, stripped)
```

macOS: config + documented steps only for now — see [dist/README.md](dist/README.md).
