# ACP integration: shared harness + Grok Build (2026-08)

## Decision
- Add an **ACP harness** (`crates/harness/src/acp/`) speaking Agent Client Protocol
  v1 — JSON-RPC 2.0 newline-framed over stdio, same wire shape as the codex
  app-server — over the shared `crates/harness/src/jsonrpc.rs` client (promoted
  from `codex/rpc.rs`). Wire types are hand-rolled tolerant serde against raw
  `Value`s (house style, verified against `agent-client-protocol-schema` 1.3.0),
  NOT the official SDK crates: zeron keeps its own child-lifecycle hardening
  (StderrTail, SIGTERM→SIGKILL, PATH composition) and shell-script test
  fixtures, and drives raw updates the SDK's `ActiveSession` abstraction hides.
- First registered agent: **Grok Build** (`grok agent stdio`), xAI's native ACP
  agent (npm `@xai-official/grok`, ACP registry id `grok-build`). Auth: browser
  OAuth or `XAI_API_KEY`; zeron passes env through. `GROK_EXECUTABLE` overrides
  resolution (tests point it at `tests/fixtures/fake-acp.sh`).
- **claude/codex converted to ACP too** (2026-08-08, wing's call: "keep things
  clean"): `AcpHarness::claude()` via `@agentclientprotocol/claude-agent-acp`
  (pinned 0.66.0) and `AcpHarness::codex()` via `@agentclientprotocol/codex-acp`
  (pinned 1.1.14), resolved from PATH or via the managed install (below).
  The bespoke stream-json/app-server adapters (~4,300 lines) are deleted; the
  catalogs (models, effort clamping, Ultrathink prefix) survive as spec inputs.
  Accepted deltas: Claude steering is now priority-`now` pre-emption (adapter
  semantics) instead of step-boundary stdin; sandbox policy control is
  adapter-owned; zeron-specific settings ride config options where advertised
  (mode → bypassPermissions, model via family-alias matching — the claude
  adapter advertises SDK aliases like `opus[1m]`/`sonnet`/`haiku` —
  fastMode/thinking as booleans) and are silently skipped elsewhere
  (ultracode has no adapter surface today). AskUserQuestion arrives as a
  question-shaped `session/request_permission` (options without allow/reject
  kinds) and bridges to the input panel; allow/reject-shaped requests
  auto-accept. Per-turn usage comes from the settled prompt response.

- **Hermes + Pi registered** (2026-08-08): `AcpHarness::hermes()` runs Nous
  Research's native ACP server (`hermes acp`; Python install via
  `curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash` plus the
  `.[acp]` extra — no npm fallback, so resolution is PATH/`~/.local/bin`/
  `~/.hermes/bin` only, `HERMES_EXECUTABLE` overrides). No
  `_session/steering` extension and no effort config advertised (Hermes 4's
  hybrid reasoning is model-internal) → turn-boundary steering, empty ladder;
  the model list is discovered over ACP (below), with the Nous flagships as
  the static fallback. `AcpHarness::pi()` runs the pi coding
  agent (pi.dev) through the community `pi-acp` adapter (pinned 0.0.33,
  managed-install fallback; requires the pi CLI itself,
  `@earendil-works/pi-coding-agent`; `PI_ACP_EXECUTABLE` overrides). Models
  ride pi's own provider config (catalog advertises a `default` pass-through
  entry); thinking ladder minimal→max maps onto zeron's levels via the
  generic `thought_level` preference ladder ("off" has no zeron tier).
- **Devin registered** (2026-08-15): `AcpHarness::devin()` runs Cognition's
  native ACP server (`devin acp`; install via
  `curl -fsSL https://cli.devin.ai/install.sh | bash` or
  `brew install --cask devin-cli` — no npm fallback, so resolution is
  PATH/`~/.local/bin`/homebrew bins only, `DEVIN_EXECUTABLE` overrides; the
  agent is also in the official ACP registry, id `devin`). No
  `_session/steering` extension → turn-boundary steering. No `thought_level`
  config option — effort is baked into the advertised model ids
  (`claude-5-fable-high`), so the descriptor ladder stays empty; the model
  list is discovered over ACP (below) with two flagship rows as the static
  fallback. Unattended parity rides the mode select's `bypass` value (added
  to the generic no-prompts preference list). CLI 3000.6.14 also exposes
  subagent lifecycle and child transcript updates when the client advertises
  `cognition.ai/subagentSupport`; Zeron correlates the child agent id back to
  the parent's `run_subagent` tool-call id and routes the tagged messages,
  thoughts and tools into a nested transcript. The separate
  `cognition.ai/subagentControl` extension stays disabled until Zeron has a
  foreground/background control surface.
  Resume quirks verified live (3000.6.14): `session/load` replays the full
  history (dropped, the doc has it) and then continues normally; a turn that
  was cancelled leaves its user message in Devin's history with no reply, so
  the next prompt after a resume may answer BOTH the new prompt and the stale
  one (SWE-1.7 did exactly that). `session/load` also refuses with
  `-32015 "already open in another process"` while a previous `devin acp`
  still holds the session — the shared driver's fresh-session fallback covers
  that, at the cost of Devin-side context.
- **ACP is the source of truth for model lists** (2026-08-08; preference
  order inverted 2026-08-09): `models()` runs a short-lived probe
  (initialize → `session/new`, the `discover_commands` pattern) and reads
  the `model` config option's choices FIRST, falling back to the legacy
  first-class `models` state (SessionModelState) only when no model config
  option is advertised. The original paseo order (legacy state first) put
  one row per model × effort in the picker: codex-acp enumerates
  `gpt-5.6-sol[low]`…`[ultra]` on the deprecated surface while its
  `configOptions` carry base ids with effort as a separate `thought_level`
  select — the same reason Zed deleted its model-API support outright
  (zed@c413552859). 1M-context variants (`opus[1m]` display form,
  `claude-…-1m` SDK form) collapse into the base row's Context Window trait
  when the bare id is also advertised, and stand alone otherwise. Traits
  come off the wire too: every select/boolean config option outside
  mode/model/thought_level becomes a `ModelOption` (codex `fast-mode` — its
  service tier — and `collaboration_mode`; `currentValue` doubles as the
  default choice), so the static catalogs' option sets now only apply on
  the legacy-fallback and probe-failure paths. The catalog still enriches
  matched ids (label/description/curated per-model ladders); discovered
  models otherwise inherit the probe session's `thought_level` ladder plus
  the spec's `ladder_extras` (Claude Ultrathink — a prompt-prefix mode the
  wire can't advertise). Revisit a Kimi-style per-model walk if an agent
  genuinely varies ladders per model.

## Protocol surface used (v1)
- `initialize` (protocolVersion 1; fs/terminal client capabilities declined) →
  `session/new` / `session/load` (fresh-session fallback; replay drained, the
  doc already holds history) → `session/prompt` per turn; the prompt RESPONSE
  carries the `stopReason` (`cancelled` → Interrupted, `refusal` → Errored,
  else Completed). `session/cancel` is the interrupt; SIGTERM/SIGKILL escalate.
- `session/update` notifications → `AgentEvent`: message/thought chunks →
  Text/ReasoningDelta; `tool_call`/`tool_call_update` → typed ToolCall (kind +
  rawInput + locations + diff content) + ToolResult carrying **capped output
  text and inline diffs** (16KB/64KB harness caps; 4KB/16KB doc caps in
  `parts.rs` — the session-load-size discipline); `plan` → `ToolCall::Todo`
  (stable id `acp-plan`); `available_commands_update` →
  `AgentEvent::AvailableCommands`. `usage_update` is a context gauge, not
  per-turn tokens — deliberately unmapped.
- `session/request_permission` → auto-accept the preferred allow option
  (`allow_always` > `allow_once` > first) — parity with claude
  bypassPermissions / codex approvalPolicy never. Question-shaped means
  *some option lacks an allow/reject kind* — kinds legitimately repeat
  (codex sends TWO `allow_always` options on every exec approval: "Allow
  for Session" + a prefix-rule amendment; treating duplicates as questions
  re-prompted the user for every command, 2026-08-09). The forced
  no-prompts mode matches per-adapter naming: claude `bypassPermissions`,
  codex `agent-full-access`.
- **Session config options**: ACP has no per-prompt model field; the run's
  model + reasoning apply through `session/set_config_option` against the
  session response's advertised `configOptions` (category `model` /
  `thought_level`, matched to advertised value ids, skipped when current,
  never fatal). Grok's effort ladder in the picker is Low/Medium/High →
  `low`/`medium`/`high`; other zeron levels degrade down a preference ladder
  (`config_option_sets`).
- Steering: `_session/steering` extension when
  `initialize._meta.steering.supported` (org adapters); request carries
  `_meta.steering.idleBehavior: "promptRequired"` so a turn-end race hands the
  text back instead of firing an untracked turn. Without the extension (Grok):
  queue and deliver as the next `session/prompt` — `SteeringMode::TurnBoundary`.
  Session parks between turns while the steering mailbox lives (codex pattern).
- Ordering hazard fixed twice: responses resolve via the pending map while
  notifications ride the incoming channel, so (a) `request_draining` flushes
  the channel after `session/load` resolves, (b) the turn arm drains queued
  updates before emitting Done, (c) EOF right after a final response reads as
  a clean finish (50ms turn-future grace), not a crash.

## New shared surface
- `Harness::commands()` (default empty) + `ListCommands` RPC (mirrors
  ListModels, relay-forwardable) → composer `/` popup (mirrors the file-mention
  popup; local `filter_indices` ranking, no per-keystroke RPC).
- `AgentEvent::ToolResult{output?, diff?}` → `MessagePart::Tool{output?, diff?}`
  → doc columns (`output`, `diff` — additive, TS mirror updated in
  render-parts.ts/control-types.ts) → expandable transcript chips
  (`tool_detail_lines`: `similar` line diff, context collapsed to `⋯`, 12-line
  cap, analytic heights).

## Managed adapter installs (2026-08-15)
- The `npx -y <pinned>` fallback put every user's npm state in the chat hot
  path and was the root of the "harness protocol error … exit code 254" class
  of report (zeronsh/comet#95): npm encodes fatal fs errors as `256 - errno`
  exits (254 = ENOENT, 243 = EACCES — npm/cli#4838), often with no stderr,
  and a cold `npx` could also stall a first chat for minutes while it
  downloaded the adapter's dependency tree (claude-agent-acp's is ~570MB).
- Replaced by `adapter_install`: pinned packages install ONCE into
  `~/.zeron/adapters/<pkg>/<version>` (`$ZERON_ADAPTERS_DIR` overrides) with
  a zeron-owned npm cache beside them, atomically (tmp dir + bin-entry
  verification + `.zeron-install-ok` marker + rename), then every launch is
  `node <entry>` directly. Discovery probes never block on npm (background
  install + static-catalog fallback); `run()` blocks, and install failures
  carry npm's full output plus the decoded errno. The engine prewarms
  adapters for present CLIs at boot. Handshake additionally bounded at 120s
  (a hung agent used to spin "Working" forever).

## Citations
agentclientprotocol.com (v1 spec + schema), agentclientprotocol org repos
(claude-agent-acp v0.66.0, codex-acp v1.1.14 — steering wire shape),
agent-client-protocol-schema 1.3.0 (serde tags), ACP registry entry
`grok-build`, live `grok agent stdio` initialize handshake (2026-08-07).
