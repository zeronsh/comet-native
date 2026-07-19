# Comet — Complete Feature Inventory (parity checklist for the native rewrite)

Source: exploration of ~/github/comet on 2026-07-19. Rewrite keeps ONLY the Loro path; token-usage
display EXCLUDED. File paths refer to the reference repo.

## 0. Architecture orientation
- Backend = engine, UI = viewport. Headless backend runs agents, owns auth, hosts terminals,
  computes diffs, syncs. Desktop is a thin viewport over localhost WebSocket "IPC" (UiRpc).
- Two transports, one contract: UI<->backend = IPC (UiRpc); backend<->backend = device-room relay
  DO on the edge (ControlRpc). WebRTC mesh is GONE (replaced by relay).
- KEEP (Loro/DO): transcripts = per-chat Loro docs via session-room DO; commands
  (run/steer/interrupt/respondInput) = durable doc entries executed by the chat's host.
- DROP (Orbit): messages sync over Postgres logical replication, device-addressed run RPCs,
  WatchMessages, WebRTC + signaling.
- NOTE: chats/sessions/devices/checkout-diffs entity sync is STILL Orbit in comet — the rewrite
  must re-home it (decision: per-org workspace Loro doc, see ARCHITECTURE.md).

## 1. Desktop app
### 1.1 Window shell
- 1320x880 (min 900x600), frameless-inset title bar, traffic lights {14,15}, opaque #0a0a0a bg.
- External links open in OS browser; single-instance lock; dev vs packaged ports (26634/26654) and
  data dirs (.comet-dev/.comet). Fullscreen hides traffic lights -> cluster reflows.

### 1.2 App phases (App.tsx)
- Three phases with crossfade: Gate (loading/reconnect/login card over bg-grid), OrgGate ("Create
  your workspace" + existing org memberships + "Use a different account"), app (router).
- Boot Splash: comet-wave loader, fades out (splash-out) once connected && authReady (15s cap),
  never returns mid-session.

### 1.3 Shell layout (__root.tsx)
- Sidebar column + main panel + optional right "Changes" pane; only middle outlet swaps.
- Left sidebar: collapsible + drag-resizable (208-400px, default 256, persisted). Collapse animates
  transition-[width] 200ms ease-out to 0; main panel goes full-bleed (margins/radius/border melt).
  Double-click separator resets; arrow keys nudge +/-16px.
- Right Changes pane: session-scoped open flag, drag-resizable 360-760px default 520 capped 52%,
  width transition 200ms; inner content full-width while outer clips (no reflow mid-toggle).
- Header h-11, 3 keyed variants (settings title / chat title + Remote badge + show-changes /
  empty drag strip). Bottom fade: overlay gradient (not mask — preserves scrollbar).
- Reserved status strip (h-6) for WorkingIndicator so composer never shifts.
- Container queries: transcript gutters/message-rail collapse below ~48rem.

### 1.4 Keyboard shortcuts
- Customizable (localStorage): Mod+S toggle sidebar, Mod+B toggle changes, Mod+J toggle terminal.
- Shift+Up/Down steps session list in visual order; Enter send, Shift+Enter newline; Esc cancels
  recording; question panel: 1-9 select, Enter submit, Back.
- Settings page: click-to-record combo, conflict detection, per-row Reset + Restore defaults.

### 1.5 Routes
- / -> redirect last-used/local/first device. /device/:id -> "Send a message to start" canvas.
- /device/:id/chat/:chatId -> Conversation. /settings: Profile (EXCLUDED heatmap), devices
  (registry, rename, presence dot, copy id), agents (accounts), shortcuts, archived (unarchive).

### 1.6 Sidebar
- DeviceSwitcher + "New session" button + session list + UserMenu (name/email/plan, settings, signout).
- Flat or grouped-by-project (persisted); "Show more" per group.
- Session rows: title, time, status dot (staleness-checked), context menu Rename/Archive/Delete,
  hover-intent prefetch, optimistic pending chats deduped on sync.
- View Transitions on resort: per-row viewTransitionName, 260ms cubic-bezier(0.22,1,0.36,1) glide.
- Edge fades via CSS mask; no scrollbar.

### 1.7 Composer
- New-chat mode: harness/repo/branch/worktree/model/reasoning/options. Existing-chat: send + tweaks.
- Compact<->expanded auto-flip (canvas text metrics vs pill capacity; newline; narrow column
  MIN_COMPACT_INPUT_WIDTH=200). Textarea never remounts (caret survives). Auto-grow 76-260px.
- Send / Steer (mid-run with steerable harness; "steers the current run") / Stop (red square when
  nothing typed or non-steering). Enter/Shift+Enter.
- Optimistic echo (client-minted id shared with persisted row, dedup); on failure prompt +
  attachments + error return to draft slot.
- Drafts & attachments per chat across navigation; drag-drop/paste images, file picker,
  AttachmentStrip, preview dialog.
- Notices: amber (offline device) / red (send failure).
- QuestionPanel replaces composer while run awaits input: paged 1-at-a-time "1/3", single-select
  auto-advances after 220ms, multi/typed explicit, number keys, free-text override, wizard state
  cached per requestId, latched across blips.
- Pickers: HarnessModelPicker (harness rail + models, harness locked once chat exists),
  TraitsPicker (reasoning + advertised model options; trigger shows non-defaults "High · 1M · Fast"),
  RepoPicker (search, Open folder… in-app browser w/ breadcrumbs + keys + skeletons, Clone from
  URL…, Create new repo…), BranchPicker (search + isolated-worktree toggle ~/.comet/worktrees/…).

### 1.8 Conversation / transcript
- ONE transcript source = doc projection. Optimistic echoes until first doc frame.
- Mugen virtualizer, maxW 46rem, wide gutters. User: right bubble radius 16 + attachments + hover
  timestamp. Assistant: markdown (monochrome theme), streaming rows fade in; consecutive tools ->
  ToolGroup fold; InputChip, ErrorChip. Fixed-height Escape wrappers for chips/timestamps.
- Stick-to-bottom: 70px band, wheel-up releases, own-send re-engages + smooth-scroll, resize
  correction.
- MessageRail: left minimap of user prompts; active tick brightens, hover grows + preview card,
  click smooth-scrolls; hidden < 48rem.
- ToolGroup: summary ("Ran 3 commands · edited 2 files"), Collapse tween, open while
  streaming+trailing, click pins. ToolChip: icon+label+one-line detail per tool kind, guide rail,
  error tint. Attachments thumbnails loaded from owning device.

### 1.9 Accounts settings
- Provider cards (Claude Code, Codex): avatar, email, usage meters (rate-limit windows,
  indigo->amber>=80%->red>=95%, reset time), Active/plan badges, Switch/Forget.
- Add-account: paste-code (Claude) or browser-wait poll (Codex). Skeletons; device switcher
  retargets which device's logins are shown (mesh-forwarded).

### 1.10 Terminal panel
- xterm.js equivalent needed. Session-scoped tabs, restored on return; PTYs on owning device
  (detach != close). Tab drag-reorder (sliding transforms 150ms), middle-click close, new-tab,
  hide (Cmd+J). Height drag 160px-55vh, 200ms transition.
- Input coalescing 12ms, resize debounce 80ms, reconnect backoff, "[process exited N]",
  #090909 bg + full ANSI palette. Bounded 1MB replay window.

### 1.11 Changes / diff viewer
- Unified patch -> file/hunk/line/notice rows; per-file Collapse 180ms, chevron rotate 200ms;
  +/- gutters; syntax highlight via time-sliced tokenizer (6ms budget); header "N Uncommitted
  changes / +a / -d / Partial snapshot"; states preparing/error+last/clean/list.
  Resolves by chat.checkout_id or device+cwd.

### 1.12 Working indicator & animation catalog (NO framer-motion; all CSS)
- WorkingIndicator: gradient matrix spinner + rotating flavour word (20 words / 7s, seeded) +
  elapsed; "Sending…" bridge; staleness-checked; survives reload; shows for remote runs.
- fade-in 0.5s cubic-bezier(0.16,1,0.3,1) translateY 4->0 (entrances)
- splash-out 0.5s opacity+translateY -6, 0.15s delay
- comet-pulse 2.4s infinite staggered cell opacity 0.08->1 scale 0.9->1
- gradient-spin-pulse per-cell phase wave, 750ms
- menu-in 0.14s scale 0.96 + translateY -2 (popovers, transform-origin tracks anchor)
- dialog-in 0.18s scale 0.96->1
- fade-quick 0.15s; view-transition resort 260ms; many 200ms width/height/margin/colors eases
- prefers-reduced-motion disables cell animations
- Theme: always-dark monochrome, Geist / Geist Mono, oklch neutral scale, hairline white borders,
  glass-surface (blur 44 saturate 1.8 brightness 1.18), bg-grid, drag regions, thin scrollbars.

### 1.13 State & connection
- Every UI action = RPC to local backend; send path QueueCommand (doc). Chunked uploads 60KB.
- Subscriptions: AuthStatus, WatchDevices, WatchChats(scoped), WatchSessions, WatchCheckoutDiffs,
  LocalDevice, per-active-chat WatchDocMessages; hover-prefetch; reconnect from scratch on drop.

## 2. Control plane (RPC surface)
### ControlRpc (device-addressed, relay-forwardable; optional targetDeviceId)
- ListHarnesses, ListModels{harness}
- Run(RunRequest)->RunAck{runId}; Subscribe{sessionId,afterSeq?}->stream SessionStreamEvent;
  Interrupt{runId?|sessionId?}; Steer{sessionId,prompt}->{accepted}; RespondInput{sessionId,
  requestId,answers}
- ListRepos/AddRepo{path}/CloneRepo{url}/CreateRepo{name}; ListFolders{path?}; ListBranches{repo};
  CreateWorktree{repo,branch}; DeleteWorktree
- UploadChunk/UploadCommit; ReadAttachment(stream)/ReadAttachmentChunk
- OpenTerminal{chatId,cols,rows}->TerminalSession; SubscribeTerminal{id,afterSeq?}->stream
  (Data{seq,data}|Exit{seq,code,signal?}); WriteTerminal; ResizeTerminal; CloseTerminal
- ListAgentAccounts{forceUsage?}; ActivateAgentAccount; ForgetAgentAccount; StartAgentLogin ->
  {loginId,url,mode:paste-code|browser}; CompleteAgentLogin{code}; PollAgentLogin; CancelAgentLogin
### DataRpc (IPC-only)
- WatchDevices/WatchChats{deviceId?}/WatchSessions{deviceId?}/WatchDocMessages{chatId}(KEEP)/
  WatchCheckoutDiffs -> streams. (WatchMessages, WatchUsage DROPPED.)
- QueueCommand{chatId,kind,payload}->{commandId}
- Mutate: CreateChat, SetChatConfig, SetChatArchived, RenameChat, RenameDevice, DeleteChat,
  MarkChatSeen
### AuthRpc (IPC-only)
- AuthStatus -> stream (SignedOut|NeedsOrganization{user}|SignedIn{user,orgId?})
- SignIn->{url}; SignInHeadless->{url}; CompleteSignIn{code}; SignOut; ListOrgs; CreateOrg; SelectOrg
### Wire types
- AgentEvent: SessionStarted, TextDelta, ReasoningDelta, ToolCall, ToolResult, Usage(kept as event,
  not displayed), Error, InputRequested, InputResolved, Steered, Done
- ToolCall kinds: Exec/ReadFile/WriteFile/EditFile/ApplyPatch/Search/Glob/WebFetch/WebSearch/
  Todo/Mcp/Unknown
- Model/ModelOption/ModelOptionChoice, RunRequest, Chat/Session/Message/Device/Repo/Worktree/
  FolderListing/CheckoutDiff, AgentAccount/AgentUsage, AuthState

## 3. Backend engine
- 3.1 Lifecycle: single-instance lock on data dir; login-shell PATH; crash shield (drain+exit);
  parent-PID watchdog; heartbeat 15s; device registration; stale-session recovery; headless
  sign-in prompt on TTY; doc command executor (host-only) run/steer(->new-turn fallback)/
  interrupt/respondInput.
- 3.2 Sessions engine: per-session pub/sub; Subscribe = journal replay (resumable seq) then live;
  persistent steerable sessions (idle reaper, STALL_MS=10min watchdog, steering mailbox delivered
  at step/turn boundary); run-journal on disk (auto-resume after crash); withDoc hooks at every
  transcript boundary; DocSegmentWriter streams folded parts at STREAM_COMMIT_MS; recovery stamps
  aborted.
- 3.3 Session-docs host: docs.sqlite (snapshots + processed-command ledger); per-chat handle joins
  wss /session/{chatId}/ws (loro protocol + ephemeral); writes user/assistant entries; drains
  durable commands (processed-id idempotence, mark-BEFORE-execute); publishes diff sidecar;
  presence; warm-opens recent chats (14d, cap 30); cold via device-room nudge; L2 tail GET.
- 3.4 Terminals: pty; bounded 1MB replay; owner re-checked; live shells survive detach; exited
  buffers 30min TTL; max 32; SubscribeTerminal replays then tails.
- 3.5 Repos/diffs: list/add/clone/create; branches; worktrees ~/.comet/worktrees/<repo>/<name>;
  checkoutIdentity; CheckoutDiffSync: fs watchers + 2min repair, git diff (name-status + numstat +
  patch incl untracked), 3MiB cap, sha256; publishes DiffSidecar to chat DOs; GitMetadataSync
  watches HEAD -> chat.branch; folder listing in disposable worker w/ 6s timeout.
- 3.7 Auth: WorkOS auth-code + loopback callback; headless paste-code; refresh persisted; org
  gate; dev mode bearer. Uploads: chunked staging -> durable file. Agent accounts: claude-swap
  (Keychain "Claude Code-credentials" / ~/.claude/.credentials.json + ~/.claude.json; Codex
  $CODEX_HOME/auth.json); detect live, swap to activate, plan labels, usage probes, OAuth flows.
  Device-room host: one wss to device DO; serves full ControlRpc via virtual sockets over
  {s,k,to,from} frames; nudges. Peers: dials other devices' relay DOs, link caching.

## 4. Harness
- HarnessShape: id, name, supportsSteering, steeringMode (step-boundary|turn-boundary),
  reasoningLevels, models, run(request, controls) -> AgentEvent stream.
- RunControls: requestInput(questions)->answers (blocks agent); steering mailbox.
- HarnessId: claude-code | codex | cursor. ReasoningLevel: minimal..ultra + ultracode + ultrathink
  (prompt prefix). Sandbox: read-only|workspace-write|danger-full-access.
- Claude adapter behaviors to replicate: model discovery, effort ladders, context-window option
  ([1m] suffix), fast mode, always-thinking models, AskUserQuestion -> requestInput, steering via
  persistent streaming input, system:init dedup, subagent frame filtering, rate-limit events.
- Codex: app-server JSON-RPC (thread/start, sendUserMessage, sandbox policy, service tier).
- Cursor: turn-boundary steering. Mock for tests.

## 5. Session doc schema (MUST stay shape-compatible with TS packages/session-doc)
- meta LoroMap {chatId, schemaVersion=1} (host-only writer).
- messages LoroList<LoroMap{id, role, parts: LoroList<part maps>, createdAt, deviceId, status?,
  continuationOf?}> keyed by id. (tokens field dropped in native.)
- DocMessagePart {id, kind: text|tool|input|error, text?(LoroText), call?(RenderToolCall),
  isError?, questions?, resolved?, message?}. TEXT BODIES IN LoroText (1.03x oplog vs 125x).
- commands LoroList<LoroMap{id, kind, payload, issuedBy, issuedAt, basedOn?, expiresAt?, status,
  resolution?}>. Rules: (1) each device appends only its own immutable entries; (2) host sole
  outcome writer (composer may cancel own pending); (3) evaluateCommand: processed-id dedupe ->
  skip; TTL -> expired; newer same-kind steer/interrupt -> superseded; interrupt past basedOn.turnId
  -> superseded; else execute. TTL 24h.
- splitMessageEntry at part boundaries (text chunked by code points) so no op > MSG_INLINE_MAX
  256KB; continuationId = "root#cN"; joinContinuations render-time inverse.
- render-parts policy: RenderToolCall keeps command/path/pattern/url/query/todo/server/tool; DROPS
  WriteFile content, Edit old/new, WebFetch prompt, Mcp/Unknown input. Full inputs only in host's
  local run journal.
- Sidecars: SessionTail (last-64 joined + total), DiffSidecar (patch + summaries + adds/dels +
  truncated, 3MiB).
- Constants: MSG_INLINE_MAX 256KB, RETAIN_DAYS 30, COMPACT_LOG_BYTES 8MB, SOFT_CEILING 25MB,
  STREAM_COMMIT_MS 120, DO_FLUSH_MS 5000, LRU 80MB, TAIL 64, TERMINAL_OUTPUT_BATCH_MS 12.

## 6. Edge (Cloudflare Worker + DOs — stays TypeScript)
- Worker routes: /health; /session/:chatId/ws (loro room); /tail/:chatId; /stats/:chatId;
  /diff/:chatId GET/POST; /snapshot/:chatId; /append/:chatId; /seed/:chatId;
  /device/:deviceId/ws?role=host|client&connId=; /device/:id/sidecar/:name; /device/:id/status;
  /device/:id/nudge; /attachments/:sha256 PUT/GET/HEAD (content-addressed R2 att/{user}/{sha},
  32MB, server-side hash verify).
- Auth: WorkOS JWKS at edge; DOs see only Worker-stamped identity; ownership claim-on-first-join.
- SessionRoom DO: hibernatable WS, updates log (5s flush), snapshot blob, lazy tail, diff blob,
  %EPH presence; two-level compaction (log-fold @8MB lossless; history-trim shallow snapshot at
  daily frontier checkpoints >= RETAIN_DAYS); daily alarm -> checkpoint + trim + R2
  backup/{chatId}/latest.loro; VV backfill on join; fragment reassembly; stale-peer import ->
  InvalidUpdate ack -> app re-submit; blobs chunked over DO SQL.
- DeviceRoom DO: byte-pipe frames uleb128 len ‖ JSON {s,k,to?,from?} ‖ payload; one live host
  socket; relay control frames; durable nudges (replayed on host join, dedup, cap 256); sidecar
  slots (repos snapshot); /status /nudge.

## 7. Server -> folded into edge for native
- Keep: WorkOS code exchange/refresh, org list/create (move to edge Worker routes).
- Drop: Orbit push/query, Postgres, signaling, Fly deploy of separate server.

## 8. EXCLUDED (token usage display)
- WatchUsage RPC, UsageStats/UsageDay, backend usage aggregation, profile heatmap/hero/stats,
  per-message token columns, doc `tokens` field.
- Kept as separate concern: agent-account RATE-LIMIT usage meters (CLI plan quotas) and the Usage
  AgentEvent passthrough (harness-level, not persisted).
