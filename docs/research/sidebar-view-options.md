# Sidebar view options and conversation links

Status: implemented and verified locally
Branch: `wip/sidebar-view-options`
Base: `origin/main` at `92b5732` (`ci: add permanent internal TestFlight workflow (#213)`)
Research date: 2026-08-22

## Goal

Make source-control metadata in the sidebar describe the conversation it belongs
to, add a small set of persistent sidebar view options, and add right-click copy
actions for both Zeron and the conversation's actual harness.

This plan deliberately keeps the project selector as a scope control. View
options change presentation inside that scope; they do not silently retarget the
new-session canvas or the selected project.

## Current failure

The registry shape makes `Chat.branch` look conversation-owned, but its writer is
checkout-owned:

- `crates/engine/src/diff_sync.rs::sync_entry` captures one checkout snapshot,
  then writes that snapshot's branch to every chat registered against the
  checkout.
- `crates/ui/src/change_requests.rs::desired_watch_targets` requires a non-empty
  `Chat.branch`, but keys the live watch by device and cwd.
- `change_request_for_chat` checks branch equality against that checkout
  snapshot, so all chats sharing the main checkout converge on the same branch
  and PR.
- The new-chat picker can provisionally stamp a selected/current ref, which is
  why the row often gains metadata only after the branch picker or checkout path
  has been exercised.

This is internally consistent as *live checkout state*, but misleading as
*conversation metadata*. A shared checkout changing branch is real; rewriting
the historical identity of every conversation that ever used it is not.

## Reference findings

### Codex desktop

Static inspection of the installed ChatGPT/Codex app
(`com.openai.codex`, v26.818.31338) found:

- “Organize sidebar” keeps organization (`By project`, conditional
  `By connection`, `In one list`) separate from sorting (`Priority`,
  `Last updated`, `Manual order`). These preferences are persisted at sidebar
  scope, not per task.
- Branch and PR lookup is conversation-scoped. The lookup includes the row's
  conversation id, host, cwd, branch, and origin URL. Codex does not need to
  print the raw branch on every row; the captured branch primarily powers the
  compact PR status.
- Right-click uses `Copy > Copy deeplink` and copies
  `codex://threads/<conversation-id>`. Public sharing is a separate feature and
  should not be conflated with an application deeplink.

### T3 Code

At [`pingdotgg/t3code@11f0513`](https://github.com/pingdotgg/t3code/tree/11f051373e79b38fa16f3ec1af825f5164907c1b):

- Branch and worktree are persisted on each thread shell, not derived from the
  repository's current branch. [Thread shell contract](https://github.com/pingdotgg/t3code/blob/11f051373e79b38fa16f3ec1af825f5164907c1b/packages/contracts/src/orchestration.ts#L449-L474)
- Each row queries source-control status through that thread's environment and
  cwd/worktree. Live PR state is gated by the thread branch; a mismatch is
  represented rather than painted onto every row. [Row lookup and gating](https://github.com/pingdotgg/t3code/blob/11f051373e79b38fa16f3ec1af825f5164907c1b/apps/web/src/components/Sidebar.tsx#L792-L918)
- The current sidebar keeps project scope separate from its lifecycle/inbox
  organization. The legacy “Sidebar options” menu offers project/thread sorting
  and a visible-row limit. [Legacy sidebar options](https://github.com/pingdotgg/t3code/blob/11f051373e79b38fa16f3ec1af825f5164907c1b/apps/web/src/components/LegacySidebar.tsx#L2602-L2705)
- Sidebar and chat-header menus share one action builder. Its Copy submenu
  currently exposes path, branch, and thread ID; it has no provider conversation
  link yet. [Shared action builder](https://github.com/pingdotgg/t3code/blob/11f051373e79b38fa16f3ec1af825f5164907c1b/apps/web/src/components/threadActionMenu.logic.ts#L46-L140)

### Hermes Desktop

At [`NousResearch/hermes-agent@667c787`](https://github.com/NousResearch/hermes-agent/tree/667c787a1c4b332ea763fb24910268fbd5f7a219):

- The view menu has an unusually clear taxonomy: Grouping, Ordering, Show, then
  Filters. [Option vocabulary](https://github.com/NousResearch/hermes-agent/blob/667c787a1c4b332ea763fb24910268fbd5f7a219/apps/desktop/src/app/chat/sidebar/filter-menu.tsx#L74-L113)
- Branch identity is captured on the individual session when it starts/resumes,
  explicitly avoiding attribution of the main checkout's transient branch to
  past sessions. [Backend rationale](https://github.com/NousResearch/hermes-agent/blob/667c787a1c4b332ea763fb24910268fbd5f7a219/hermes_state.py#L6505-L6537)
- PR state is keyed by repository root and the session's recorded branch, with
  a per-session override for work that moved to another branch/worktree.
  [PR store](https://github.com/NousResearch/hermes-agent/blob/667c787a1c4b332ea763fb24910268fbd5f7a219/apps/desktop/src/store/pull-requests.ts#L8-L78)
- PR lookup is lazy and remote-aware: it runs only when PR badges/filtering are
  requested and uses the checkout-owning host's `gh` capability.
  [Sidebar lookup](https://github.com/NousResearch/hermes-agent/blob/667c787a1c4b332ea763fb24910268fbd5f7a219/apps/desktop/src/app/chat/sidebar/index.tsx#L782-L840)
- Hermes registers `hermes://` and routes `hermes://open/<session-id>` into a
  session, although its sidebar currently copies only the bare ID.
  [Deep-link resolver](https://github.com/NousResearch/hermes-agent/blob/667c787a1c4b332ea763fb24910268fbd5f7a219/apps/desktop/src/lib/hermes-open-target.ts#L68-L108)

## Decisions

### 1. Separate conversation source context from live checkout state

Add an optional, explicit conversation-owned source record rather than
continuing to overload the legacy `Chat.branch` field:

```text
ConversationSourceContext
  checkout_id     canonical device-scoped checkout identity
  repo_root       host-resolved repository root
  cwd             actual run cwd/worktree
  branch          observed branch, if attached
  head_sha        observed HEAD, if available
  observed_at     capture timestamp
```

The host captures this after a queued run's worktree directive is materialized
and before the harness starts/resumes. A later turn may refresh *that chat's*
context if the user intentionally continued it after changing the shared
checkout. Filesystem/diff watchers may continue publishing live checkout state,
but must stop rewriting conversation source records.

The composer-selected ref remains a provisional first-frame label. The host's
observed context is authoritative once the first run starts.

Migration is conservative:

- Existing rows start with no trusted conversation source context.
- A uniquely isolated worktree may be backfilled when checkout identity and
  branch are unambiguous.
- Shared-main-checkout `Chat.branch` values are not promoted automatically;
  the next run/resume captures the truth for that conversation.
- The legacy scalar fields remain readable for older engines during a bounded
  compatibility period, but new UI branch/PR rendering prefers the explicit
  context.

### 2. Resolve PRs from conversation context

Change the PR demand/cache key from “current device + cwd” to a canonical
repository identity plus the conversation's recorded branch, still routed to
the owning device. Conversations recorded on the same repository+branch may
share one lookup; unrelated conversations on the same mutable cwd may not.

Rules:

- A conversation override wins when a harness/tool flow explicitly creates or
  discovers a PR on another branch.
- Otherwise resolve by recorded repo+branch.
- No source context means no badge, not a guess from the active checkout.
- A live checkout mismatch may appear in tooltip/detail state, but it does not
  replace the conversation's association.
- Keep the existing host-local `gh`, sanitized RPC, TTL, and last-success
  behavior from PR #116.
- Start/retain PR watches only when `Show pull requests` is on or a PR filter
  needs them.

### 3. Keep scope and view controls separate

The existing All projects/project selector remains the prominent scope control.
Add one quiet view-options button beside it, persisted in device-local
`ui-settings.json`.

Focused first version:

```text
Organize
  By device
  In one list

Sort
  Last updated
  Created

Show
  Branch
  Pull request
  Harness
```

Project scope belongs exclusively to the project selector; the legacy
`By project` organization value normalizes to `In one list`. Defaults preserve
today's presentation (`In one list`, `Last updated`, Branch
on, Pull request on, Harness on). The view menu mirrors the project selector:
it uses the same sidebar-inner width and spacing conventions, but aligns its
trailing edge to the view-options button. Organizing deduplicates project/device
context into section headers; it does not change the selected project, open
tabs, or new-session target. Section collapsed state is local to the selected
organization mode.

Follow-ups, not first-slice requirements:

- Priority sort (needs input/unread first) once its stable-order behavior is
  specified.
- Manual order and custom sections.
- Status and PR-state filters.
- Compact/comfortable/detailed density; Hermes correctly treats this as an
  appearance preference rather than overloading organization.

### 4. Use a shared, capability-driven Copy submenu

Build one session action model and render it from the sidebar right-click menu;
the chat header can adopt the same model later. The menu contains:

```text
Copy >
  Zeron deeplink
  <Harness> conversation link   (only when verified/supported)
  Session ID
```

Use explicit items instead of making one action place surprising multi-line
content on the clipboard. This still gives both links, and makes the distinction
between app navigation and public sharing visible.

The resolver is bound to the row, never the globally selected harness:

```text
ConversationLinks
  zeron: required internal deeplink
  external: optional { label, url }
```

Initial provider support:

- Codex: `codex://threads/<harness_session_id>` (verified in the installed app).
- Hermes: candidate `hermes://open/<encoded-session-id>`; enable only after the
  ACP session id is verified to match the desktop route id end to end.
- Claude Code, Cursor, Grok, Pi, and OpenCode: omit the external item until each
  adapter can produce a verified link. Never synthesize a URL from an opaque id.

The Zeron URI must include an opaque workspace/profile locator in addition to
the durable chat id so local, synced, and development profiles cannot collide.
Proposed shape:

```text
zeron://open/chat/<percent-encoded-chat-id>?workspace=<opaque-locator>
```

“Copy Zeron deeplink” is complete only when the URI round-trips into the correct
chat. That requires URL-scheme registration, cold-start argument/event parsing,
profile-aware chat resolution after bootstrap, and an actionable error when the
link names an unavailable local profile. Do not ship a clipboard action that
copies an unhandled URI.

## Implementation slices

### Slice A — conversation source truth

1. Add the optional source-context wire/doc model in `zeron-proto` and
   `zeron-doc`; keep old rows readable.
2. Capture the actual git context in the host command drain after worktree
   creation and before dispatch.
3. Remove branch fan-out from `diff_sync::sync_entry`; retain checkout diff and
   `checkout_id` upkeep as live checkout responsibilities.
4. Re-key change-request demand and mapping around recorded repo+branch.
5. Render branch/PR from the new context, with a conservative legacy fallback
   only for unambiguous worktrees.

### Slice B — view options

1. Add typed `SidebarOrganization`, `SidebarSort`, `show_branch`, and `show_pr`
   settings with serde defaults and compatibility tests.
2. Add the anchored view menu beside the project scope control.
3. Derive one presentation model before rendering so grouping, ordering, FLIP
   keys, archived counts, and keyboard traversal consume the same order.
4. Make PR watch demand follow `show_pr` (and future PR filters), instead of
   polling metadata the UI cannot expose.

### Slice C — links and shared actions

1. Define a pure session-action/link model with exact clipboard payload tests.
2. Add a workspace-scoped Zeron URI builder/parser and inbound route intent.
3. Register and handle `zeron://` in macOS packaging first; add Linux desktop
   entry and Windows registration with their packaging work rather than
   claiming unsupported platforms.
4. Add harness conversation-link capability, beginning with verified Codex.
5. Render `Copy` as a submenu in the existing session context menu and provide
   brief copied feedback without closing over the currently selected chat.

These are separate reviewable behaviors. If implemented as OSS contributions,
prefer three focused branches/PRs rather than one broad sidebar rewrite:

```text
origin/main -> conversation-source-context -> sidebar-view-options
origin/main -----------------------------> conversation-deeplinks
```

The source-context slice is the dependency for correct branch/PR display. The
deeplink slice can be developed independently if it reads existing chat and
harness-session identity only.

## Verification

Source-context regression fixture:

1. Create chat A and run it on `main` in the shared checkout.
2. Change that checkout to `feature/a`; create/run chat B.
3. Verify A still reads `main` and B reads `feature/a`.
4. Change the checkout again without running either chat; neither row changes.
5. Resume A; only A adopts the newly observed branch.
6. Resolve different PRs for A and B by recorded branch; verify no badge fans
   out by cwd.

View tests cover settings migration/defaults, scope remaining independent,
stable section/row keys, archived counts, and sort/group derivation. Link tests
cover URI encoding, profile collisions, unavailable profiles, unsupported
harnesses, exact Codex URI output, and menu-open snapshotting so copying from a
background row never uses the selected chat.

Run the narrow checks first:

```bash
cargo fmt --all -- --check
cargo test -p zeron-proto
cargo test -p zeron-doc
cargo test -p zeron-engine change_request
cargo test -p zeron-ui change_request
cargo test -p zeron-ui shell
```

Broaden to `cargo test -p zeron-engine`, `cargo test -p zeron-ui`, and finally
`cargo test --workspace` in proportion to the implemented slice.

## Coordination

PR [#181, pull request dashboard](https://github.com/zeronsh/comet/pull/181)
currently overlaps `crates/ui/src/shell.rs` but is a separate product surface.
Do not fold its dashboard into this contribution. Rebase the implementation
branch on the current `origin/main` immediately before coding and, if #181 has
landed, adapt the sidebar footer/menu without duplicating its route or provider
model.

## Non-goals

- Replacing the existing project filter or tab model.
- Building the pull-request dashboard from #181.
- Public/shareable transcript links or access-control changes.
- Inferring external harness URLs from undocumented opaque ids.
- Mobile parity in the first desktop slice.
