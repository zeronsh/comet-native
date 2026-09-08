# Cline harness: implementation tasks

Companion to [`cline-harness.md`](./cline-harness.md) (feasibility analysis, 2026-08).
Path chosen: **`AcpAgentSpec` over `cline --acp`** — the same shared-`AcpHarness` shape as
Grok/Hermes/Pi. Tasks are ordered; each is independently compilable. Check off as landed.

## T1 — proto: `HarnessId::Cline` variant
- [x] Add `Cline` to `pub enum HarnessId` (`crates/proto/src/agent.rs`), serde kebab-case →
      `"cline"` (additive, wire-compatible).
- [x] Grep-verify every `HarnessId` match site compiles (compiler forces exhaustive
      matches; new arms mirror Hermes/Pi exactly).
- **Done when**: `cargo check -p zeron-proto -p zeron-harness` passes with only the new
  arms added.

## T2 — harness: `cline_spec()` in `crates/harness/src/acp/mod.rs`
- [x] `AcpAgentSpec` mirroring `grok_spec()` / `hermes_spec()`:
      executable `cline`, args `["--acp"]`, env override `CLINE_EXECUTABLE`,
      npm package pinned (`cline@<minor>`), extra paths = npm-global + homebrew +
      version-manager bins, install hint → `npm install -g cline`.
- [x] `models`: static catalog fallback (Cline-curated Anthropic/OpenAI/Gemini/OpenRouter
      flagships), reasoning ladder `Minimal|Low|Medium|High|XHigh` via the
      `--thinking` mapping (`none`→Minimal).
- [x] `steering_mode: TurnBoundary` (no `_session/steering` documented);
      `prompt_complete_extension: false` until a live probe proves otherwise;
      `prompt_stall` ~30s.
- [x] `AcpHarness::cline()` constructor + `installed()` probing (PATH, login-shell PATH,
      npm/fnm/nvm/volta/pnpm/bun dirs — reuse the existing helpers).
- **Done when**: `cargo test -p zeron-harness acp` passes.

## T3 — harness: fixture tests
- [x] Extend `crates/harness/tests/acp.rs` with a cline spec case pointed at the existing
      fake-ACP shell fixture (`CLINE_EXECUTABLE` override): handshake, session/new, prompt
      settle, permission request bridge, config-option delivery.
- **Done when**: `cargo test -p zeron-harness` is green.

## T4 — engine: registry + accounts
- [x] `register_lazy` descriptor in `crates/engine/src/registry.rs` (name "Cline",
      turn-boundary steering, the T2 ladder, `installed` from the probe).
- [x] `"cline"` CLI-name mapping in `crates/engine/src/agent_accounts.rs`
      (grep `HarnessId::Hermes =>` for the full surface).
- **Done when**: `cargo test -p zeron-engine registry` passes.

## T5 — app wiring: `apps/zeron/src/main.rs`
- [x] `Ok("cline") => HarnessId::Cline` in the harness-name parser.
- **Done when**: `cargo check -p zeron` passes.

## T6 — UI surfaces
- [x] Harness description in `crates/ui/src/settings/harnesses.rs` ("Cline — open-source
      coding agent (`cline` CLI); plan/act modes, MCP, checkpoints. Install: `npm i -g cline`.").
- [x] Icon/mark + tint in `crates/ui/src/pickers.rs` and
      `crates/ui/src/settings/accounts.rs` — monochrome mark tinted by the surface
      (house pattern for harnesses without a strong brand color).
- **Done when**: `cargo check -p zeron-ui` passes; picker renders Cline when installed.

## T7 — docs + verification trail
- [x] Record live-probe results (turn settle, config options, `session/load`) in
      `cline-harness.md` once a real `cline` CLI is exercised against the fixture/spec.
- [x] Live end-to-end pass against the real CLI: session/new → session/prompt (stopReason end_turn) → cross-process session/load + follow-up turn. Full `scripts/e2e-smoke.sh` run deferred to pre-PR CI.

## Out of scope (deliberate)
- Sandbox ladder mapping (no Cline equivalent — restricted levels fall back to Cline's
  permission gating, same accepted delta as Cursor).
- Token-usage display (excluded project-wide).
- `--json` NDJSON and desktop file-IPC approval surfaces (rejected alternatives).
