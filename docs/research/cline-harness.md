# Cline harness integration: feasibility analysis (2026-08)

## Verdict

**Feasible, low-to-medium effort (~1–2 days), via the shared `AcpHarness`.** The Cline CLI
ships a first-class ACP server (`cline --acp`) built ground-up on the Agent Client Protocol —
the same integration shape as Grok, Hermes, and Pi, and (since the 2026-08 ACP conversion)
every other agent zeron drives. No new harness driver is needed; this is a new
`AcpAgentSpec`, a registry entry, and a `HarnessId` variant.

## What Cline CLI is (2026-08, v3.0.x)

Cline (npm `cline`, Apache-2.0, Cline Bot Inc.) is an open-source coding agent distributed
as a CLI with **prebuilt native binaries** (macOS/Linux/Windows, arm64+x64 — the npm package
resolves the platform binary via optional dependencies; no Node runtime required at install
time, but the binary is Node-runtime-based internally). The CLI shares its agent core with
the Cline VS Code extension, JetBrains plugin, and `@cline/sdk`, so plan/act modes, MCP
servers, checkpoints, rules, and provider configuration behave identically across surfaces.

Integration-relevant surfaces, per the official docs (`docs.cline.bot` `cli-reference`,
`usage/acp`, and the npm README):

- **ACP mode** — `cline --acp` speaks Agent Client Protocol v1 (JSON-RPC 2.0 over stdio).
  Verified clients: Zed, JetBrains AI Assistant, Neovim (CodeCompanion ships a built-in
  `cline_cli` adapter), Emacs agent-shell. Cline is in the official ACP client/agent
  ecosystem, not an adapter-mediated afterthought.
- **Over ACP it advertises**: client-driven sign-in (Cline account OAuth, ClinePass, ChatGPT
  subscription, or BYO API key), **plan/act session modes** switchable from the client's
  mode selector, **model + provider selection** from the client's picker, **permission
  prompts** routed through the client's approval UI (nothing auto-approved by default),
  **session resume** (conversations persist; clients supporting session loading can restore
  a thread), **images** in prompts for vision models, and org switching for team accounts.
- **Headless JSON mode** — `cline --json "..."` streams NDJSON events, one-shot only.
- **Auto-approval** — `--auto-approve <bool>` (default `true` headless, **`false` in ACP
  mode**); an ACP config-options toggle ("Auto-approve tools") exists per session.
- **Thinking budget** — `--thinking none|low|medium|high|xhigh` (default `medium`).
- **Other flags** — `-P/--provider <id>`, `-m/--model <id>`, `-k/--key`, `-c/--cwd`,
  `--id <session-id>` (resume), `--data-dir` (isolated state), `--retries`.
- **Env wiring** — `CLINE_API_KEY`, `CLINE_PROVIDER`, `CLINE_MODEL` (pin defaults),
  `CLINE_DATA_DIR`. OAuth providers fail fast without saved credentials in non-interactive
  runs (no hidden browser flow) — good headless hygiene.
- **Providers**: Cline (usage-billing), Anthropic, OpenAI/Codex OAuth, Gemini, OpenRouter,
  Bedrock, Vertex, Cerebras, Groq, Ollama/LM Studio, any OpenAI-compatible endpoint.

## Where it fits in zeron

The `Harness` trait (`crates/harness/src/lib.rs`) reduces to: spawn child, speak the agent's
own wire, map to one normalized `AgentEvent` stream ending in `Done`, with a steering
mailbox, input-request bridge, and cancellation. Since the ACP conversion
(`docs/research/acp.md`), the shared `AcpHarness` (`crates/harness/src/acp/`) is the house
driver for everything — including claude/codex/cursor via pinned community adapters. Cline
needs **no adapter**: it speaks ACP natively.

### Proposed spec: `AcpHarness::cline()`

Mirrors `grok_spec()` / `hermes_spec()` (`crates/harness/src/acp/mod.rs`):

| Field | Value | Notes |
|---|---|---|
| `id` | `HarnessId::Cline` | New proto variant; serde kebab-case → `"cline"` (additive, wire-compatible) |
| `display_name` | `"Cline"` | |
| `executable` | `"cline"` | |
| `args` | `["--acp"]` | Nothing else needed — no leader/daemon mode, no auto-update flag documented |
| `env_override` | `"CLINE_EXECUTABLE"` | House pattern for tests + custom installs |
| `npm_package` | `Some("cline@<pinned>")` | Pin a minor; Cline releases fast (336 versions) and ACP surface may drift |
| `extra_paths` | npm-global + version-manager bins | Reuse `npm_global_bins` + `node_version_manager_bins` helpers (fnm/nvm/volta/pnpm/bun) — cline installs exactly where those probes already look |
| `steering_mode` | `TurnBoundary` | No `_session/steering` extension documented; same as Grok/Hermes/Pi |
| `reasoning_levels` | `[Minimal, Low, Medium, High, XHigh]` | Maps Cline's `--thinking` ladder; deliver via ACP config options or session modes if advertised, else skip (the spec effort machinery already degrades gracefully — Hermes ships an empty ladder) |
| `models` | Cline's provider catalog | See model discovery notes below |
| `prompt_complete_extension` | **verify live** | Whether Cline echoes `_meta.promptId` on completion; default `false` keeps zeron's turn-end handling + quiesce watchdog as backstop |
| `prompt_stall` | ~30s like Grok | zeron-side stall bound; `ZERON_<id>_STALL_MS` env knob |

### Engine + UI registration (mechanical, precedent-rich)

- `crates/engine/src/registry.rs`: `register_lazy` with a static `HarnessDescriptor`
  mirroring the spec (turn-boundary steering, the ladder above, `installed` probed).
- `crates/proto/src/agent.rs`: `HarnessId::Cline` variant.
- `crates/engine/src/agent_accounts.rs`: `"cline"` CLI-name mapping.
- `crates/ui`: harness picker description, mark/icon, settings copy — same edits Grok and
  Hermes each needed (grep `HarnessId::Hermes` for the full edit surface).
- `apps/zeron/src/main.rs` harness-name parsing (`Ok("cline") => HarnessId::Cline`).

### Run-request mapping

- `cwd` → child spawn cwd (already harness-generic).
- `auto_approve: true` → pass `--auto-approve true` at launch or flip the ACP
  config-option; `false` is Cline's ACP default, so permission requests arrive as
  `session/request_permission` and bridge to zeron's existing approval UI unchanged.
- `resume` → ACP session loading (Cline documents persistent sessions; verify the
  `session/load` method works with its persisted ids).
- `attachments` → ACP image content blocks (Cline documents image support over ACP).
- `sandbox` → **no Cline equivalent** (it has no readOnly/workspace-write ladder). Map
  `DangerFullAccess` freely; for the restricted levels fall back to Cline's default
  permission gating (auto-approve off), which zeron's approval UI already mediates. Same
  accepted-delta class as the Cursor harness.

## Risks and open questions (verify against a live CLI before merging)

1. **Turn-end determinism.** The critical reliability property. Cline is ACP-native, which
   historically beats adapter-mediated ACP (the claude/codex adapters manufactured
   done-status bugs), but zeron must still verify: does a prompt's JSON-RPC response settle
   exactly at turn end, both user-prompted and agent-initiated? Until verified, keep the
   quiesce watchdog (`deterministic_turn_end() == false`) — zero code cost, just a label.
2. **Thinking ladder delivery.** `--thinking` is a CLI flag; whether Cline exposes it as an
   ACP config option (Zed's "Auto-approve tools" toggle suggests config options are
   supported) or only at process launch is undocumented. If launch-only, the ladder applies
   to new sessions via argv and mid-session switches degrade to the default. Probe
   `session/new` + config options live.
3. **Model discovery.** Cline's provider/model surface is richer than most (provider
   switching mid-pick). Over ACP it advertises "any model from the active provider's
   catalog, or switch providers" — expect the standard ACP model list; the static spec
   catalog is the fallback. Auth lives in `~/.cline` (`cline auth`), reused automatically —
   zeron passes no credentials, matching the Grok/Hermes posture. `CLINE_PROVIDER` /
   `CLINE_MODEL` env can pin defaults if the picker gets noisy.
4. **Version pinning + auto-update.** Cline ships a `cline update` command and nightly
   channel; ACP surface drift is possible. Pin `npm_package` and re-verify on bumps, as
   with the claude/codex adapters (0.66.0 / 1.1.14 pinned).
5. **Session resume semantics.** Documented as "clients that support session loading can
   restore a thread" — verify `session/load` + id stability across CLI restarts, and that
   zeron's continuation (resume-on-restart) path composes.
6. **Usage accounting.** Token usage is explicitly out of zeron's parity scope; per-turn
   usage from the settled prompt response (as with the other ACP agents) is sufficient.

## Alternatives considered and rejected

- **`cline --json` NDJSON one-shot**: the `codex exec --json` situation — fine for CI, no
  steering, no approval bridge, no session continuity. Not a chat-harness surface.
- **Desktop file-IPC approval** (`CLINE_TOOL_APPROVAL_MODE=desktop`): request/decision JSON
  file schema is undocumented; only useful if ACP permission requests prove broken.
- **`@cline/sdk` (Node)**: violates the house rule — device-side is Rust, no Node runtime
  dependency; the SDK duplicates child-lifecycle/PATH work zeron already owns.

## Implementation checklist

1. Live probe against `cline --acp` (latest stable + pinned): initialize handshake,
   `session/new`, `session/prompt` settle behavior, permission request shape, config
   options (auto-approve, thinking), `session/load`, model list. Record findings here.
2. `HarnessId::Cline` in proto + serde test.
3. `cline_spec()` in `crates/harness/src/acp/mod.rs` + shell-script ACP fixture tests
   (`crates/harness/tests/acp.rs` pattern, `CLINE_EXECUTABLE` → fixture).
4. Registry descriptor + agent-accounts CLI name + UI surfaces (picker, settings, icon).
5. e2e: `zeron headless` + real Cline run through the `scripts/e2e-smoke.sh` shape.

No engine, sync, doc-crate, or edge changes are required — the entire integration is
harness-crate + registration, which is why the estimate stays low.


## Live probe results (2026-09-01, cline 3.0.60, verified against the real CLI)

Probes: JSON-RPC over stdio against `cline --acp` (handshake, session/new,
session/prompt, cross-process session/load). Findings:

1. **Turn-end determinism: CONFIRMED.** `session/prompt` settles with a first-class
   `stopReason: "end_turn"` (4.1s for a trivial turn). The prompt response is the
   authoritative turn end; no prompt-complete extension exists (`_meta` is null) and
   none is needed.
2. **Config options: provider + model only.** `session/new` advertises exactly two
   `configOptions` (category "model"): `provider` (cline / cline-pass / openai-codex
   with currentValue) and `model` (the full catalog, currentValue
   `anthropic/claude-sonnet-5`). **No** thinking/ladder option and **no** auto-approve
   option: the reasoning ladder degrades to the agent default exactly as designed, and
   tool approvals ALWAYS arrive as `session/request_permission` (ACP default) and bridge
   to zeron's approval UI. `RunRequest::auto_approve` cannot be honored per-session over
   ACP; launching with `--auto-approve true` would disable approvals for every session —
   deliberately not done.
3. **First-class models + modes.** `session/new` carries a full `models.availableModels`
   catalog (`currentModelId`), and `modes` with plan/act (`currentModeId: "act"`) —
   zeron's picker receives the real catalog, and the plan/act switch maps onto the
   session modes rather than the static spec.
4. **Session resume: CONFIRMED cross-process.** `agentCapabilities.loadSession: true`;
   `session/load {sessionId, cwd, mcpServers: []}` on a persisted session from an exited
   CLI process succeeds and replays the prior conversation as `session/update`
   (`user_message_chunk` / `agent_message_chunk`) before a follow-up prompt settles
   contextually. Note: `mcpServers` is REQUIRED on `session/load` (omitting it yields
   -32602), and loading an in-process active session errors with -32002.
5. **Updates observed**: `agent_message_chunk`, `user_message_chunk`,
   `session_info_update` — all standard ACP update shapes zeron's normalizer handles.
   `promptCapabilities.image: true` (attachments map to image content blocks).
6. **No steering extension** (`_meta` null): turn-boundary steering stands, as spec'd.

Spec deltas from the original table: none required — `prompt_stall` 30s, turn-boundary
steering, and the degraded ladder are all validated behavior. Open follow-up: cline
installed via **mise** (`~/.local/share/mise/installs/node/latest/bin`) is reachable
only through the login-shell PATH probe; consider adding mise's bin dirs to the shared
version-manager scan (`node_version_manager_bins`).
