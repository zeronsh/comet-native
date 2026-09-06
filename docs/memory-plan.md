# Memory plan — bounded RSS without touching feel

Written 2026-08-03 after a full-codebase audit plus empirical benchmarks (below).
Problem: Activity Monitor shows Zeron at 450–600MB on viewer-only laptops and
>1GB after heavy use. Target: ~150–250MB steady-state, flat over a workday.

## 1. What we measured

Engine benchmarks ran the real `zeron headless` debug binary (v0.1.12) with the
mock harness, offline, on Linux/glibc; RSS sampled from `/proc`. Scripts:
`/tmp/zeron-mem-test/{run,bench2,bench3}.sh` (to be promoted to
`scripts/mem-smoke.sh`, phase 0).

| Measurement | Result |
|---|---|
| Engine idle, no docs | **32MB** |
| Boot with 6 chats on disk (docs lazy-load) | 35MB — nothing opens at boot |
| Retention per streamed chat (~330KB text) | +1.4–7.7MB, **never released** (0 bytes back after 60s idle) |
| Retention per reopened chat (no streaming) | +1.2–2.8MB — the rest of the streamed number is allocator watermark |
| Streaming 1.6MB-text chat | **+18.6MB resident (≈11.6× raw text)** |
| Idle creep, 6 docs open, 3min | +0.2MB — growth is event-driven, not timer-driven |
| Cold-open small chat (snapshot→first frame) | **18ms** (warm re-watch: 17ms) |
| Cold-open 1.6MB chat | **62ms** (warm: 51ms) — delta ≈ 11ms, debug build |
| Watch frame size (what's re-serialized per 120ms tick, ×~4 copies) | 227KB small chat / 1.13MB big chat |
| Loro snapshot on disk | 6–13KB (columnar+compressed; mock text overly compressible — treat as lower bound) |

Code-audit findings backing each work item are cited inline (file:line refs
verified 2026-08-03).

**The two load-bearing empirical facts:**
1. Cold-open from the SQLite snapshot is within ~11ms of a warm doc, even for a
   large chat, even in a debug build. Doc eviction is therefore *not* a feel
   trade — the speed of opening chats comes from having local state on disk,
   not from unbounded residency.
2. Nothing is ever released, so RSS is monotonic in chats-ever-touched,
   images-ever-viewed, and terminals-ever-opened — plus a malloc watermark
   (macOS libmalloc never returns small-span pages) fed by ~4 full-transcript
   copies per 120ms streaming tick.

## 2. Feel budgets (regressions gated on these)

- Chat switch between recent (warm) chats: unchanged — they stay pinned.
- Reopen of an evicted chat: first paint < 100ms (measured 62ms debug worst
  case today); live deltas may land up to ~200ms after paint (one room RTT).
- Streaming smoothness: unchanged or better (phase 2 removes per-tick work).
- Reattach-after-detach: unchanged — engine keeps hosting docs
  it has run/command obligations for.

## 3. Phase 0 — measurement first

- Promote the bench scripts to `scripts/mem-smoke.sh`: boot baseline, N-chat
  stream, reopen latency, idle-flatness; assert thresholds; run in CI (Linux).
- macOS one-pager in the script header: `footprint -p <pid>` / `vmmap
  --summary` to split MALLOC vs IOSurface/Metal when a report comes in.
- Every phase below lands with before/after mem-smoke numbers in the PR.

## 4. Phase 1 — zero feel risk

| Item | Change | Evidence | Expected |
|---|---|---|---|
| Allocator | mimalloc (or jemalloc) as global alloc in `apps/zeron` | system malloc watermark; churn sources below | watermark becomes recoverable; biggest single lever on macOS |
| Image lifecycle | Call gpui `remove_asset` when transcript rows drop / adopt an LRU image cache with byte budget; bound the global encoded-bytes cache `attachments.rs:462` (no eviction today); clear staged attachments on chat delete | decoded RGBA + atlas tile + encoded bytes ≈ 2.5× decoded size per image, permanent; one screenshot ≈ 48MB decoded | 100MB+ on image-heavy use |
| Doc delete eviction | `DeleteChat`/`DeleteSpace` drop the doc handle, close the room, delete the snapshot row (`doc_host.rs:125` handles map is insert-only; `rpc.rs:636-693` leaks) | audit §1 | correctness + a few MB per deleted chat |
| Bound channels | `RpcClient::subscribe` unbounded (`client.rs:123`) → conflating/bounded (watch semantics are latest-wins); terminal PTY + subscriber channels (`terminals.rs:211,243`) → bounded with drop policy; offline local-update queue (`room.rs:341`, drains only on connect) → byte cap + full-resync on overflow | leak-shaped under slow consumer / disconnect | removes the balloon modes (dev builds, sleep/wake, firehose terminals) |
| Hygiene | clear codex `streamed_text` per turn; evict idle journal fds; prune `dial_locks` | audit §3/§9/§7 | small, stops slow creep |

## 5. Phase 2 — bounded docs + delta streaming (the structural fix)

1. **Doc LRU** — wire the dead `DOC_LRU_BYTE_BUDGET` (80MB,
   `crates/doc/src/constants.rs:17`). Pins: selected chat; chats this device
   hosts with a live run or undrained commands; N most-recent. Evict = flush
   snapshot (already 1Hz-debounced), drop `ChatDocHandle` + mirror, close
   room. Measured reopen cost: +11ms vs warm. This alone caps the growth term
   that took the home laptop to 557MB with zero local sessions.
2. **Lazy mirror** — `messages_tx` holds a full transcript copy per open doc
   even with no subscriber (`doc_host.rs:142,229`); materialize only while a
   watch is attached.
3. **Delta doc-watch** — `WatchDocMessages` currently re-serializes the whole
   transcript per 120ms commit through 4 copies (`engine/src/rpc.rs:775` →
   `rpc/src/client.rs:139` → `ui/state.rs:902` → `transcript.rs:1199`;
   measured 1.13MB/frame on a 1.6MB chat). Send per-entry deltas. Also fixes
   streaming CPU — same pipeline as the remote-streaming chunkiness work.
4. **Fold O(n²)** — `fold_event_into_parts` clones the whole parts vec per
   event (`parts.rs:85`); mutate in place. `render_parts` clone per tick
   (`sessions.rs:805`) → borrow.
5. **Incremental reads** — `read_entries`/`read_commands` do whole-doc
   `get_deep_value().to_json_value()` per tick (`schema.rs:209,233`); move to
   `doc.subscribe` diff application (the mirror layer's stated design,
   ARCHITECTURE.md §2.3). Same for `workspace.rs:291` `chat()` linear
   whole-container scan on every 120ms `is_host` check.

Expected after phases 1–2: streaming multiplication ~11.6× → ~2–3× raw text;
viewer-laptop steady state ≈ gpui baseline + selected chat ≈ 150–250MB, flat.

## 6. Phase 3 — larger surfaces, more care

- **Terminals**: purge `terminal/panel.rs:239` chats map on chat delete;
  scrollback 10k → configurable ~2k lines (24B/cell ⇒ 30–50MB per
  fully-scrolled terminal today); count replay bytes raw, not base64.
- **Diff pane**: watch carries every checkout's ≤3MiB patch, resident engine +
  UI (`diff_sync.rs:106`, `changes.rs:480`); send summaries, fetch patch on
  demand; stop the 120s repair tick re-capturing unchanged checkouts
  (`diff_sync.rs:486`).
- **Transcript render caches**: byte-budget `tree_cache`/`RenderCache`/
  `HighlightStore` to viewport±K rows (today they grow with every row ever
  scrolled past, freed only on chat switch).
- **Shallow snapshots** (deferred, correctness-sensitive): client-side trim to
  the edge's compaction frontier would cut in-memory doc 2.5–4× → ~1×; needs
  the stale-peer story (`room.rs:132` gives up rather than rebuilding).
- **Tail-first cold open** (`materialize_tail` exists unwired,
  `schema.rs:738`): paint last-64 for never-opened remote chats while the doc
  backfills. Perceived-latency win, not a memory item.

## 7. Implementation status (2026-08-03)

Phases 1–2 landed on `zeron/zeron-memory-usage-investigation` (phase 3
deliberately skipped — terminals get their own pass with the terminal bug
work). Shipped: mimalloc in both binaries; attachment-image LRU (64MB encoded
budget) with gpui asset release on eviction + staged-attachment purge on chat
delete; doc eviction on DeleteChat/DeleteSpace; doc LRU (12 warm docs / 80MB
estimate, pinned: watched, live-writer, host-pending-commands) with lazy
mirror; in-place event fold; container-scoped doc reads + single-row workspace
`chat()`; delta `WatchDocMessages` protocol (reset + per-entry upserts + text-append
ops, desync → resubscribe) across engine/UI — measured on a 1.6MB
streamed reply: 257MB of watch frames before, 2.3MB after (110×; median
frame 2.7KB, one full-entry frame at turn completion); bounded RPC stream queues (256,
backpressure); offline room queue drained during backoff; codex/journal-fd/
dial-lock hygiene. Verified: full workspace suite green; 20-chat run shows the
LRU evicting (8×), post-cap growth slope halved, and RSS recovering at idle —
which the baseline never did. Cold-open stayed on the measured ~62ms path.

**Allocator revision (2026-09-03):** the phase-1 mimalloc adoption itself
became the top residency cause — `mimalloc = "0.1"` had drifted onto crate
0.1.52, whose bundled default is mimalloc **v3.3.2**, and v3 retains the
streaming/render churn as permanent RSS instead of returning it (measured on
Linux: daemon at 908MB for 7.3MB of docs after six streamed chats, no idle
recovery; glibc control flat on identical workloads; the ratchet surfaced as
"memory grows unbounded when scrolling/switching sessions"). Fix: mimalloc is
now macOS-only (the libmalloc watermark it was adopted for) and pinned to the
crate's `v2` feature (mimalloc 2.3.2, ~half of v3's retention); Linux runs the
system allocator. Purge knobs (`MIMALLOC_PURGE_DELAY=0`,
`MIMALLOC_ABANDONED_PAGE_PURGE=1`) measurably do NOT rescue v3.

Known follow-ups: GPU atlas tiles for raw-bytes images still free only on
window close (needs a small gpui-fork patch exposing a drop path for
`ImageSource::Image`); UI-side full-transcript clone per frame
(`transcript.rs` sync) could move to Arc-per-entry; boot-time warm-open of
recent chats (PARITY gap) is unchanged.

## 8. Acceptance

- Viewer laptop after browsing 20 chats incl. images: **<250MB** (from ~600).
- RSS flat ±10% over 8h mixed use (no monotonic ratchet).
- mem-smoke thresholds in CI: engine idle <40MB; stream-retention <3× raw
  text; reopen p95 <100ms; idle creep <1MB/10min.
- No feel-budget regression (§2), verified per landing PR.
