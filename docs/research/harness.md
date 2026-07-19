# Rust harness integration: Claude Code + Codex (2026-07)

## Decision
- Claude Code: spawn installed `claude` CLI, speak stream-json directly. NO crates.io SDK dep
  (crate "claude-agent-sdk" is name-squatted w/ fake anthropics repo; `claude-codes` 2.1.x is a
  reasonable serde-types reference to vendor from). Python SDK source = authoritative wire spec.
- Codex: spawn `codex app-server`, JSON-RPC 2.0 over stdio — port comet's codex.ts (which already
  bypasses the SDK). Only option with token deltas + turn/steer + turn/interrupt + thread/resume +
  model/list + approval requests. codex-rs crates are NOT published (git dep not recommended).
  `codex exec --json` = CI-only surface (no deltas/steer/approvals).

## Claude CLI protocol
- One-shot: `claude -p "<prompt>" --output-format stream-json --verbose --include-partial-messages [--bare]`
  (--bare skips hooks/skills/CLAUDE.md/MCP auto-discovery; will become default for -p).
- Steerable: add `--input-format stream-json`, keep stdin open.
  - stdin user turn: {"type":"user","message":{"role":"user","content":"..."},"parent_tool_use_id":null}
    — steering = another such line mid-run (consumed at step boundary).
- stdout frames (JSONL):
  - system/init: model, tools[], cwd, session_id, capabilities[] (v2.1.205+; feature-detect here,
    e.g. interrupt_receipt_v1)
  - system/api_retry: error categories authentication_failed|oauth_org_not_allowed|billing_error|
    rate_limit|overloaded|invalid_request|model_not_found|max_output_tokens|server_error|unknown
  - stream_event: raw API deltas (content_block_delta -> text_delta/thinking_delta); has
    parent_tool_use_id (subagent frames non-null -> filter)
  - assistant / user messages (tool_use / tool_result blocks), rate_limit_event
  - result: subtype success|error_*, usage, session_id (last line)
- Control channel (bidirectional control_request/control_response, request_id-multiplexed):
  - client->CLI: initialize, interrupt, set_permission_mode, set_model, rewind_files,
    mcp_reconnect/toggle/status, get_context_usage, stop_task; model discovery is a control req.
  - CLI->client: can_use_tool {tool_name, input, permission_suggestions...} — reply
    {"behavior":"allow","updatedInput":{...}} or {"behavior":"deny","message":...}.
    AskUserQuestion ALWAYS reaches can_use_tool -> intercept, requestInput UI, allow with
    updatedInput.answers. (Same mechanism as comet claude.ts.)
  - interrupt: control request; >=2.1.205 response carries {still_queued:[uuids]}.
- Resume: --resume=<session_id> (equals form; cwd-scoped), --continue, --fork-session.
- One-shot interrupt: SIGTERM (kills bash trees, runs SessionEnd hooks, exit 143).
- Input side de facto stable but undocumented (claude-code#24594) — pin min CLI version + gate on
  capabilities.

## Codex app-server protocol
- Handshake: initialize {clientInfo, capabilities{experimentalApi, optOutNotificationMethods}} ->
  initialized notification. Overload = JSON-RPC error -32001.
- thread/start {model?, cwd, approvalPolicy, sandbox} -> thread.id; thread/resume {threadId}
  (fallback to thread/start if rollout missing).
- turn/start {threadId, input:[{type:"text",text}], model?, effort?, sandboxPolicy, approvalPolicy};
  turn/steer {threadId, expectedTurnId, input}; turn/interrupt {threadId, turnId}.
- Notifications: turn/started|completed{usage}|failed|aborted; item/started|completed
  (item.type: agent_message, reasoning, command_execution, file_change, mcp_tool_call, web_search,
  todo_list); deltas item/agentMessage/delta, item/reasoning/textDelta|summaryTextDelta,
  item/commandExecution/outputDelta, item/plan/delta; thread/tokenUsage/updated.
- Server->client approval REQUESTS (must answer): item/commandExecution/requestApproval,
  item/fileChange/requestApproval -> {accept|acceptForSession|decline|cancel}.
- model/list {cursor?} -> supportedReasoningEfforts, service tiers (experimentalApi).
- Types: `codex app-server generate-json-schema` per installed version -> generate Rust types
  (typify) or hand-write tolerant serde (both delta field spellings, ignore unknown methods).
- Child lifecycle hardening from codex.ts to port: SIGTERM->SIGKILL escalation, signal-death !=
  clean exit, EPIPE swallowing.

## Shared shape
Both reduce to: spawn child, frame JSONL stdout (+ id-multiplexing), write stdin lines, map to one
AgentEvent enum, mpsc steering mailbox, cancellation token kills child.

## Capability matrix to replicate (from packages/harness)
Normalized AgentEvent stream; typed ToolCall decoding (Bash/Read/Write/Edit/Grep/Glob/WebFetch/
WebSearch/TodoWrite -> Exec/ReadFile/...; codex item types); model discovery + effort ladders +
options ([1m] context suffix, fastMode, thinking, service tiers); ultrathink = prompt prefix,
ultracode = xhigh + setting; sandbox mapping; AskUserQuestion -> requestInput; resume; interrupt;
steering (step-boundary via stdin / turn/steer with expectedTurnId + turn/start fallback);
subagent frame filtering; error-code mapping.
(Citations in agent transcript: code.claude.com/docs/en/headless, agent-sdk/typescript,
claude-code#24594, claude-agent-sdk-python query.py/subprocess_cli.py, Codex app-server docs +
README, openai.com "Unlocking the Codex harness", codex#5028.)
