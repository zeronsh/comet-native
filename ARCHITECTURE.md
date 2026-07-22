# comet-native — Architecture

A ground-up native rewrite of [comet](../comet) — a multi-device controller for coding agents
(Claude Code / Codex) — in Rust, with a gpui UI. Fresh app; no backwards compatibility required.

**Pillars (from the goal):**
- No Orbit. Sync is Loro CRDT docs (loro-mirror model) through Cloudflare Durable Objects.
- Durable Objects stay **TypeScript** (decision + evidence: `docs/research/durable-objects-language.md`).
  Everything device-side is Rust.
- Feature parity with comet **except token-usage display** (poor fit for CRDTs; excluded).
- Frontend is **gpui** (pinned Zed rev). Virtualization + markdown techniques ported from
  **mugen + pretext** (`docs/research/mugen-pretext.md`).
- One binary, **headed or headless**. Smooth transitions/animations matching the original
  (catalog in `docs/research/feature-inventory.md` §1.12).

## 1. Topology (unchanged shape, new materials)

```
gpui UI ─ in-proc/localhost RPC ─ engine A ══ DeviceRoom DO relay ══ engine B ─ RPC ─ gpui UI
                    │            (edge Worker: auth, rooms, R2)          │
                    └── Loro sync ── SessionRoom DO (per chat) ──────────┘
                                └── Workspace doc room (per org) ────────┘
```

- **Engine = backend** (was `@comet/backend`): runs agents, owns auth, terminals, repos/worktrees,
  diff sync, doc hosting. Pure Rust daemon, fully functional headless.
- **UI = viewport** (was Electron): gpui app rendering engine state. Talks the same typed RPC
  whether the engine is in-process or a separate daemon. Organized around **spaces** — synced
  (device, folder) pairs: the sidebar lists spaces plus a global attention-sorted Active list;
  the main area shows the selected space's sessions as horizontal tabs (closing a tab archives);
  new sessions are minted onto the space's device via relay-forwardable RPCs.
- **Edge (TypeScript, ported from comet `apps/edge`)**: Worker + SessionRoom DO (per chat) +
  DeviceRoom DO (per device) + R2 attachments + WorkOS JWKS auth. Absorbs the old `apps/server`
  responsibilities (WorkOS code exchange/refresh, orgs) so **Postgres, Orbit, the Hono server, and
  the WebRTC/signaling stack are all gone**.

### Headed / headless
Single binary `comet`:
- `comet` — headed. If a local engine daemon is already listening on the IPC port, connect to it;
  otherwise run the engine **in-process** (RPC over an in-memory duplex — same protocol, zero
  serialization shortcuts, so the boundary stays honest).
- `comet headless` — engine only; prints sign-in URL on TTY (paste-code flow), serves IPC on
  localhost + hosts its DeviceRoom for remote control. A VPS runs this; a laptop's UI drives it.

## 2. Data model — all Loro, no Orbit

Two doc kinds, one room protocol (loro-protocol over WebSocket, the same protocol the TS edge
already speaks; Rust side uses the official `loro-protocol`/`loro-websocket-client` crates or a
thin hand-rolled client over `loro` 1.13.x — verify interop early, M1 exit criterion):

1. **Session doc** (per chat) — the transcript + durable command queue. Schema is a Rust port of
   `packages/session-doc` (same container names/shapes so the edge's tail materializer keeps
   working): `meta` map, `messages` list (parts as list-of-maps with **LoroText bodies** — the
   measured 1.03× oplog shape; never LWW value rewrites), `commands` list with ledger rules 1–3
   (append-only per-device entries; host-only outcomes; dedupe/TTL/supersede evaluation).
   Continuation splitting at 256KB, render-only tool parts (full inputs stay in the host's local
   run journal), tail/diff sidecars. Constants carried over (`STREAM_COMMIT_MS=120`,
   `DO_FLUSH_MS=5s`, compaction at 8MB, retain 30d, tail 64).

2. **Workspace doc** (per org — NEW; replaces comet's residual Orbit entity sync) — **spaces**
   registry (id, deviceId, path, name?, gitDetected, checkoutId — a space is a synced
   device+folder pair, the app's unit of organization; the owning device's SpacesSync stamps git
   presence so branch pickers / the diff sidebar gate on a synced bool, no RPC), chats index
   (id, deviceId, title, archived, cwd, branch, checkoutId, spaceId, lastSeenAt,
   lastMessagePreview/At, config), devices registry (id, name, platform, lastSeenAt), session
   status rows (Working indicator; staleness-checked client-side so a crashed backend never shows
   eternal "Working"), checkout-diff summary pointers. `lastSeenAt` is the synced LWW seen marker
   behind the "completed (unseen)" indicator. Lives in its own DO room (same SessionRoom DO
   class, doc id `ws2/{orgId}` — the `2` is the spaces-overhaul destructive break), with presence
   via Loro `EphemeralStore` (replaces the 15s heartbeat writes). Writer discipline: each device
   writes only its own device/session/chat rows and the git stamps of spaces it owns;
   creates/renames/archives/seen-marks are LWW map sets from any device. `deleteSpace` cascades:
   the space row and every chat/session row in it tombstone in one commit.

   *Why a workspace doc and not N tiny docs:* the sidebar needs one subscription for the whole
   list (grouping, resort animations, unseen markers); one doc = one room connection + one mirror.
   Volume is tiny (index rows, no transcripts), so oplog growth is negligible and daily compaction
   applies anyway.

3. **Mirror layer** (`comet-doc` crate) — Rust equivalent of loro-mirror: typed structs for the
   schema, **incremental** application of `doc.subscribe` diffs into cached state (no full
   re-hydration per change — this is also what fixes comet's known O(transcript) re-projection
   inefficiency, remaining-work item 1a), and a diff-reconcile write path (evaluate `lorosurgeon`
   0.2.x as a dep; our schema is small enough to hand-roll if it doesn't fit). The UI renders
   mirror state directly with per-entry change notifications — the "endgame" the TS
   implementation documented but never reached.

### Command plane
Send/steer/interrupt/respondInput = durable command entries in the session doc (`QueueCommand`),
executed by the chat's **host** device (executor gated on chat ownership; mark-processed BEFORE
execute; steer with no live run dispatches as the next turn). Offline sends queue in the doc.
This is comet's proven design, kept verbatim.

## 3. Cargo workspace

```
comet-native/
  Cargo.toml                 # workspace
  crates/
    proto/        comet-proto    # wire types: AgentEvent, ToolCall, RunRequest, Model,
                                 # entities, RPC envelopes (serde; ndjson framing)
    doc/          comet-doc      # session-doc + workspace-doc schemas, mirror layer,
                                 # parts fold, continuations, command ledger, sidecars
    sync/         comet-sync     # loro room client (join/VV backfill/fragments/backoff),
                                 # ephemeral presence, DocsStore (SQLite snapshots +
                                 # processed-command ledger)
    harness/      comet-harness  # Harness trait + claude-code (stream-json subprocess),
                                 # codex (app-server JSON-RPC), mock; steering mailbox,
                                 # requestInput, models/reasoning/options catalogs
    engine/       comet-engine   # sessions engine (pub/sub, run journal, recovery, stall
                                 # watchdog), doc host + command executor, repos/worktrees,
                                 # checkout-diff sync, terminals (portable-pty), uploads,
                                 # agent accounts (cred swap), auth (WorkOS via edge),
                                 # device-room host/peers, identity
    rpc/          comet-rpc      # UiRpc/ControlRpc: typed req/resp/stream over WS (tokio-
                                 # tungstenite) + in-memory transport; device-room virtual
                                 # sockets ({s,k,to,from} frames)
    ui/           comet-ui       # gpui app: shell, sidebar, conversation, composer,
                                 # terminal view, diff pane, settings, animation kit
  apps/
    comet/                       # the binary (headed default, `headless` subcommand)
  edge/                          # TypeScript Worker + DOs (ported from comet/apps/edge,
                                 # + auth-exchange routes absorbed from apps/server)
  docs/                          # this file + research reports
```

Engine async runtime: **tokio** throughout; the UI bridges via `gpui_tokio` (`Tokio::spawn`
futures surfaced as gpui `Task`s). In-process mode runs the engine on its own tokio runtime
thread; the UI never blocks on it.

## 4. UI plan (gpui) — parity + smoothness

Reference: `docs/research/gpui.md`, `docs/research/mugen-pretext.md`,
feature spec `docs/research/feature-inventory.md` §1.

- **Deps**: `gpui` + `gpui_platform` pinned to one Zed rev (Apache-2.0). **We do not use Zed's
  GPL crates** (`markdown`, `ui`, `theme`, `editor`) — markdown, components, and theme are ours.
- **Transcript**: gpui `list()` + `ListState::new(n, ListAlignment::Bottom, overdraw)` (sum-tree
  offsets, follow-tail). On top of it, port the mugen behaviors that gpui doesn't give us:
  - stick-to-bottom **spring** with feed-forward tracking of streaming growth; interrupt from
    *user input* (wheel-up / drag), re-engage within a 70px band; own-send re-engages + smooth
    scrolls;
  - **block-granularity rows** (one row = one markdown block / tool group, not one message) with
    stable ids `msgId#blockId`; live turn stays unsplit, re-splits on persist; optimistic echo
    rows share the client-minted id so persistence never flickers;
  - row height memoization keyed by (row id, content length, width) so a streamed token
    re-measures one row;
  - scroll-anchor absorption for above-viewport height changes.
- **Markdown** (`comet-ui::markdown`): `pulldown-cmark` parsing on `background_spawn` with
  coalescing (Zed's proven pattern), block-level incremental re-parse of the streaming tail
  (incremark's O(delta) idea: only re-parse from the last stable block boundary), monochrome
  theme where **numbers drive layout, colors are paint**. Code blocks: monospace, no wrap ⇒
  height = lines × line-height (layout independent of highlight); syntax highlighting via
  `synoptic`/`syntect`-class tokenizer run time-sliced in the background, colors applied as text
  runs (paint-only). Streaming **fade-in veil** on newly appended text via `with_animation`
  opacity (paint-layer, never affects layout). `prefers-reduced-motion` honored.
- **Composer**: hand-rolled gpui text input (start from Zed's `examples/input.rs`: IME, selection,
  clipboard, key actions), compact↔expanded auto-flip by measured text width, auto-grow 76–260px,
  Enter/Shift+Enter, Send→Steer→Stop morph, drafts + attachments per chat, drag-drop/paste
  images, QuestionPanel (paged, 1-9 keys, 220ms auto-advance) replacing the composer while input
  is requested. Pickers (harness/model, traits, repo w/ folder browser, branch w/ worktree
  toggle) as gpui popovers with `menu-in` scale/fade.
- **Terminal**: `alacritty_terminal` (vte state machine, MIT/Apache) + `portable-pty` on the
  engine side; custom gpui grid element; tabs w/ drag-reorder (150ms sliding transforms), height
  drag 160px–55vh, 12ms input coalescing / 80ms resize debounce, 1MB replay, detach ≠ close.
- **Diff pane**: unified-patch parser → virtualized file/hunk/line rows, per-file collapse
  (180ms height tween), time-sliced highlight, 200ms width transition on the pane itself.
- **Animation kit** (`comet-ui::motion`): small helpers over gpui `Animation` reproducing the
  comet catalog — `fade-in` (0.5s, cubic-bezier(0.16,1,0.3,1), translateY 4→0), `splash-out`,
  `comet-pulse` staggered cell wave (boot splash + loaders), `gradient-spin-pulse` matrix
  spinner (WorkingIndicator + rotating flavour word), `menu-in`/`dialog-in` scale-fades, 200ms
  ease-out width/height transitions for sidebar/panes, sidebar-resort **slide animation**
  (we own the list, so animate row positions directly — the View Transitions equivalent, 260ms
  cubic-bezier(0.22,1,0.36,1)), reduced-motion switch.
- **Theme**: always-dark monochrome, oklch-derived neutral scale precomputed to Hsla, hairline
  borders, Geist/Geist Mono bundled fonts.

## 5. Engine plan

Direct ports of comet behaviors (spec: feature-inventory §3):
- **Sessions engine**: per-session broadcast hub; on-disk run journal (resumable `seq` replay,
  crash auto-resume); persistent steerable sessions (steering mailbox at step/turn boundary; idle
  reaper; 10min stall watchdog); recovery stamps `aborted`.
- **Doc host**: per-chat handle (join room, VV backfill, write user entries + stream assistant
  segments at 120ms commits, drain commands host-only with processed-ledger idempotence, publish
  diff sidecar, presence); warm-open recent chats (14d/cap 30); nudge-driven cold open; SQLite
  snapshot store.
- **Harness** (research pending — `docs/research/harness.md`): trait mirroring comet's
  `HarnessShape`; Claude Code via `claude` CLI stream-json in/out (control protocol for
  permissions/AskUserQuestion→requestInput, resume, steering); Codex via app-server JSON-RPC or
  `codex exec --json`; model/reasoning/option catalogs ported from `packages/harness`.
- **Repos/diffs**: git2 or `git` subprocess (subprocess — matches comet, avoids libgit2 edge
  cases); worktrees under `~/.comet-native/worktrees`; fs watchers (`notify`) + 2min repair; diff
  capture (patch + numstat + untracked, 3MiB cap, sha256) → workspace doc summary + DO diff
  sidecar.
- **Agent accounts**: credential-slot swap (macOS Keychain via `security-framework`, files
  elsewhere), plan labels, usage probes, paste-code/browser-poll OAuth flows.
- **Auth**: WorkOS through edge routes (`/auth/exchange`, `/auth/refresh`, orgs); loopback
  callback server headed, paste-code headless; dev mode (no key ⇒ bearer = configured user id).

## 6. Edge plan (TypeScript, `edge/`)

Port `comet/apps/edge` nearly verbatim (it is already Loro-native and smoke-tested: session room
w/ hibernation + two-level compaction + daily alarm backups, device room byte relay + nudges +
sidecar slots, R2 attachments, JWKS auth). Additions:
1. Workspace-doc rooms (`ws/{orgId}`) — same DO class, org-membership authz instead of
   claim-on-first-join.
2. `/auth/*` routes absorbed from `apps/server` (WorkOS API key in Worker secret).
3. Drop `/seed` migration path and legacy Orbit anything (fresh app).
Hibernation hygiene: no idle timers (flush timer only while dirty), auto-response ping/pong —
per `docs/research/durable-objects-language.md`.

## 7. Parity exclusions & deliberate changes

- **Excluded**: token-usage display (profile heatmap, lifetime stats, per-message token columns,
  `WatchUsage`). Rate-limit meters on agent accounts are *kept* (separate concern; probed from
  CLIs, not CRDT-synced).
- **Changed**: Orbit/Postgres/server → workspace doc + edge; Electron/React/mugen → gpui with
  ported techniques; Node harness SDKs → subprocess protocols; WebRTC → device-room relay (comet
  had already made this move); mobile app → out of scope for this repo.
- **Kept verbatim**: session-doc schema shape + constants, command ledger rules, edge DO design,
  render-parts privacy policy, UX behaviors and animation timings.

## 8. Milestones

Status legend: ✅ shipped · 🟡 shipped with named gaps (see `docs/PARITY.md`).

- ✅ **M0 Scaffold** — workspace builds; `proto`/`doc` crates with ledger + parts + continuation
  unit tests; gpui hello-window runs.
- ✅ **M1 Doc + sync core** — `comet-doc` mirror over loro 1.13; room client syncs with the edge
  running under `wrangler dev`; Rust⇄edge⇄Rust convergence test (M1 exit: two Rust peers converge
  through a real SessionRoom DO, tail endpoint serves).
- ✅ **M2 Engine core** — Claude harness end-to-end headless: `comet headless` + dev auth runs a
  turn, journal + doc writes, recovery test.
- ✅ **M3 UI core** — shell (sidebar/panes/header), transcript (virtualized, markdown, streaming,
  stick-to-bottom), composer (send/steer/stop, question panel); local chat fully usable headed.
- ✅ **M4 Multi-device** — device-room host/client virtual sockets, remote device control, workspace
  doc entity sync, WorkOS auth + org gate, presence. Proven live by `scripts/e2e-smoke.sh`:
  two headless engines against a real edge — B queues a run into the chat doc, the durable
  nudge wakes host A, A executes (mock harness), transcript + session status sync back to B.
- 🟡 **M5 Full surface** — terminals, diff pane, repo/branch/folder pickers + worktrees,
  agent accounts UI, settings (devices/shortcuts/archived), Codex harness. Gaps: composer
  attachment UI (engine upload RPCs exist), Cursor harness.
- 🟡 **M6 Polish** — wire reconciliation (proto AuthState on the wire, `LocalDevice`),
  two-device e2e smoke, keyboard map, clippy/fmt sweep, Linux packaging
  (`scripts/package-linux.sh` + release profile), macOS bundling config (`dist/macos/`,
  not executed — needs a Mac). Gaps: prefers-reduced-motion, engine hardening
  (instance lock, watchdogs), edge production deploy.

## 9. Open questions (tracked, non-blocking)

1. loro-protocol Rust client ⇄ TS edge interop — verify at M1; fallback is a ~300-line hand-rolled
   client (the frame protocol is small and we control both ends).
2. `lorosurgeon` fit for the mirror write path vs hand-rolled reconcile.
3. Cursor harness (comet has it; CLI surface for Rust TBD) — parity item, scheduled after Codex.
4. Text shaping performance for analytic row heights: gpui measures shaped text natively (Rust ⇒
   cheap), so we start with gpui `list()` measurement + memoization rather than porting pretext's
   full analytic kernel; revisit only if cold-open of huge transcripts measures slow.
