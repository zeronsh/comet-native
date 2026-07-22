# Parity checklist

Status of the native rewrite against `docs/research/feature-inventory.md`
(§1–§8), audited against the tree after M6. Legend: **done** (implemented and
tested), **partial** (core in place, listed gaps), **deferred** (intentionally
not built yet).

## §1 Desktop app

| Item | Status | Notes |
| --- | --- | --- |
| 1.1 Window shell | partial | gpui window, always-dark theme, external links via OS browser. Deferred: frameless-inset/traffic-light chrome (macOS packaging not executed), single-instance lock, dev-vs-packaged port split (env vars instead). |
| 1.2 App phases | done | Gate / OrgGate ("Create your workspace" + memberships) / app with crossfade; boot splash with fade-out cap (`ui/src/shell.rs`). |
| 1.3 Shell layout | done | Collapsible drag-resizable sidebar (208–400), right Changes pane (360–760, 52% cap), header variants, widths persisted to `ui-settings.json`. |
| 1.4 Keyboard shortcuts | done | Customizable keymap, click-to-record with conflict detection, per-row reset (`ui/src/settings/shortcuts.rs`); persisted with UI settings. |
| 1.5 Routes | partial | Native navigation instead of URL routes; devices / agents / shortcuts / archived settings pages exist. Profile page (heatmap) is an §8 exclusion. |
| 1.6 Sidebar | done | Device switcher, new session, grouped-by-project or flat, status dots (staleness-checked), row context menu (rename/archive/delete), resort glide. |
| 1.7 Composer | done | Send/Steer/Stop morph, compact↔expanded flip, per-chat drafts, optimistic echo with failure return-to-draft, QuestionPanel (paged, auto-advance, number keys), all four pickers (harness/model, traits, repo with folder browser + clone/create, branch with worktree toggle), image attachments (paste/drop/picker → strip → chunked upload to host device → `withAttachments` refs in prompt text + inline image blocks for the Claude harness; per-chat stash, failure hand-back, lightbox — `ui/src/attachments.rs`). |
| 1.8 Transcript | done | Doc-projection source, virtualized, markdown + syntax highlight, tool folding (ToolGroup/ToolChip), input/error chips, stick-to-bottom band, MessageRail minimap (hover preview, hidden < 48rem), user-bubble attachment thumbnails (112×80, read-back from owning device, 2s→15s retry ladder, seeded cache, click-to-expand lightbox). |
| 1.9 Accounts settings | done | Provider cards, usage meters with 80/95% thresholds + reset time, Switch/Forget, paste-code and browser-poll add flows, device switcher (`targetDeviceId`). |
| 1.10 Terminal panel | done | Session-scoped tabs, drag-reorder, middle-click close, height drag, replay-then-tail streams, input coalescing, ANSI emulator (`ui/src/terminal/`). |
| 1.11 Changes viewer | done | Patch → file/hunk/line rows, per-file collapse, ±gutters, time-sliced highlighting, preparing/clean/error states, checkout_id → device+cwd resolution. |
| 1.12 Motion catalog | partial | Motion kit (cubic-bezier curves, fade-in/quick, splash-out, pulse/gradient spinners, menu/dialog-in, resort glide). Gap: prefers-reduced-motion switch. |
| 1.13 State & connection | done | All subscriptions (AuthStatus, WatchDevices/Chats/Sessions/CheckoutDiffs, per-chat WatchDocMessages, LocalDevice probe); reconnect from scratch. |

## §2 Control plane

| Item | Status | Notes |
| --- | --- | --- |
| ListHarnesses / ListModels | done | Relay-forwardable. |
| Run/Subscribe/Interrupt/Steer/RespondInput RPCs | done (changed shape) | Deliberate redesign: these ride the durable doc command queue (`QueueCommand {run|steer|interrupt|respondInput}`) instead of device-addressed RPCs — same capability, offline-tolerant. |
| Repos/folders/worktrees RPCs | done | All eight methods, relay-forwardable. |
| Uploads / ReadAttachmentChunk | done | Chunked staging → durable file; path-jailed reads; R2 mirror. |
| Terminals RPCs | done | Open/Subscribe/Write/Resize/Close, forwardable. |
| Agent-account RPCs | done | Full login/activate/forget/poll surface, forwardable. |
| LocalDevice | done | `{deviceId}`; IPC-only (never forwarded). |
| DataRpc watches + QueueCommand | done | — |
| Mutate ops | partial | createChat/renameChat/setChatArchived/deleteChat/renameDevice done; markChatSeen accepted as a no-op (unseen markers UI-local); `SetChatConfig` exists on the doc layer but is not yet exposed as a Mutate op. |
| AuthRpc | done | AuthStatus emits the canonical proto shape (`{"state": "signedIn", …}`); SignIn/SignInHeadless/CompleteSignIn/SignOut/ListOrgs/CreateOrg/SelectOrg. |
| Wire types | done | `comet-proto`: AgentEvent, ToolCall kinds, models/options, entities, AuthState. |

## §3 Backend engine

| Item | Status | Notes |
| --- | --- | --- |
| 3.1 Lifecycle | partial | Device registration, presence heartbeat (ephemeral, 15s), stale-session recovery, headless TTY sign-in, host-only doc executor with steer→new-turn fallback. Gaps: single-instance lock, login-shell PATH capture, crash shield, parent-PID watchdog. |
| 3.2 Sessions engine | partial | Run journal on disk with crash recovery (aborted stamps), steering mailbox at step boundaries, doc hooks at boundaries, streamed part folding at STREAM_COMMIT_MS. Gaps: idle reaper + 10-min stall watchdog for persistent harness sessions. |
| 3.3 Session-docs host | done | docs.sqlite snapshots + processed-command ledger, mark-BEFORE-execute, room join per open chat, diff sidecar publish, cold-chat delivery both directions (nudge POST on queue for remote-hosted chats + warm-open on nudge receipt). Gap (minor): no boot-time warm-open of recent chats (14d/30) — cold chats rely on nudges. |
| 3.4 Terminals | done | PTYs, 1MB bounded replay + `afterSeq` resume, 32 max, exited 30-min TTL, live shells survive detach. |
| 3.5 Repos/diffs | done | list/add/clone/create, branches, worktrees, checkout identity; CheckoutDiffSync (fs watchers + repair pass, name-status+numstat+patch incl. untracked, 3MiB cap, sha256, sidecar publish); chat.branch upkeep from HEAD watch; folder listing with timeout. |
| 3.7 Auth / uploads / accounts / device-room | done | WorkOS code+loopback and paste-code flows, refresh persistence (0600), org gate, dev mode; chunked uploads; claude/codex credential swap with usage probes and OAuth flows; host relay (virtual sockets over `{s,k,to,from}` frames) + peer link cache. |

## §4 Harness

| Item | Status | Notes |
| --- | --- | --- |
| Claude Code adapter | done | stream-json, model discovery/effort ladders, AskUserQuestion → requestInput, steering via persistent input, init dedup, subagent filtering. **Live-verified against the real `claude` CLI 2.1.215**: doc-queued run → host executor → subprocess → streamed reply landed complete in the doc. |
| Codex adapter | done | `codex app-server` JSON-RPC (thread/start/resume, sandbox policy). |
| Cursor adapter | deferred | Parity item scheduled after Codex; no CLI surface settled. |
| Mock harness | done | Scripted event replay; powers tests + the e2e smoke. |

## §5 Session doc schema

| Item | Status | Notes |
| --- | --- | --- |
| Containers (meta/messages/commands), LoroText bodies | done | Shape-compatible with TS `packages/session-doc`; `tokens` dropped per §8. |
| Command rules (append-only, host outcome writer, evaluateCommand) | done | Processed-ledger dedupe, TTL, supersede rules. |
| Continuation splitting / joining (MSG_INLINE_MAX 256KB) | done | `split at part boundaries`, `root#cN`, render-time join. |
| Render-parts privacy policy | done | WriteFile content / Edit bodies / etc. stripped; full inputs only in the host journal. |
| Sidecars (tail, diff) + constants | done | — |

## §6 Edge

| Item | Status | Notes |
| --- | --- | --- |
| Worker routes | done | health, session ws/tail/stats/diff/snapshot/append, workspace rooms, device ws/sidecar/status/nudge, attachments (content-addressed R2, hash-verified). |
| Auth at edge | done | WorkOS JWKS verify; dev mode `user@org` bearers; DOs see Worker-stamped identity; claim-on-first-join ownership. |
| SessionRoom DO | done | Hibernatable WS, update log + snapshot, lazy tail, two-level compaction, daily alarm checkpoint/trim/R2 backup, VV backfill, fragment reassembly. |
| DeviceRoom DO | done | Byte-pipe frames, single host socket + supersede, relay control frames, durable nudges (replay on join, cap), sidecar slots. |

## §7 Server → edge

| Item | Status | Notes |
| --- | --- | --- |
| WorkOS exchange/refresh, org list/create at edge | done | `/auth/*` routes; API key stays edge-side. |
| Orbit/Postgres/signaling dropped | done | Nothing depends on them. |

## §8 Exclusions

| Item | Status | Notes |
| --- | --- | --- |
| Token-usage display dropped | done | No WatchUsage, no doc `tokens`, no profile heatmap; rate-limit meters + Usage AgentEvent passthrough kept as specified. |

## Deferred (cross-cutting)

- **Mobile app** — out of scope for the native rewrite so far.
- **E2EE** — transport is TLS + WorkOS bearers; end-to-end encryption of doc
  contents not designed.
- **Cursor harness** (§4).
- **macOS packaging execution** — config + steps in `dist/` only (needs a Mac).
- **Engine hardening**: single-instance lock, parent-PID watchdog, crash
  shield, idle reaper / stall watchdog, boot warm-open of recent chats.

## Summary

Table rows above: **39 done · 6 partial · 1 deferred** (Cursor harness), plus
the cross-cutting deferrals (mobile, E2EE, macOS packaging execution,
engine hardening) — the last overlaps the named gaps in the partial rows.
