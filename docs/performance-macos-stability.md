# macOS streaming stability follow-up

For the v0.2.43 follow-up, production-history idle measurements, and standalone
reproduction of the transient Metal driver charge, see
[idle presence and memory attribution](performance-idle-presence.md).

v0.2.41 removes the sidebar conversation-row cache introduced here because it
broke row widths and text truncation. The measurements below describe v0.2.40;
they are not performance claims for the hotfix. The crash, sync-recovery and
Metal drawable fixes remain. The earlier cached/fresh pixel checks did not catch
the incorrect geometry shared by both renders; sidebar layout checks now include
short and long titles/branches with selected and hovered rows.

This follow-up starts from v0.2.39 (`d97f4c2`). It fixes a reproduced native
scrolling abort and recovery defects found while investigating stuck sends.
It also reduces redundant UI work and the Metal window drawable pool. Animation
timelines, text effects, blur passes, resolution and motion settings are unchanged.

## Reproduced crash

The v0.2.39 main-thread abort resolves to `ListState::scroll_by` in the renderer
dependency: `list.rs:583`, `cannot seek backward`. The native composer reproduction
reaches it through `Transcript::step_own_turn` during consecutive short replies.

A prompt can be anchored below the viewport top, giving its list item a negative
offset. A forward animation step can then still target a position before the tree
cursor's current item. Choosing `seek_forward` from the direction of the animation
is incorrect. The renderer now compares the target with the cursor's actual item
start, using a full seek when needed. A regression test reproduces the old panic
and checks movement in both directions across the item boundary.

Finder-launched panics now include their location and backtrace in the existing
rotating app log. This supplements macOS reports whose release stacks lack symbols.

## Stuck-send recovery

Observed logs contained updates parked on missing causal history. A contiguous
server row number does not establish that a CRDT update's dependencies exist.
Previously the client could persist an advanced cursor even though snapshot export
omitted the parked update, allowing a restart to skip it indefinitely.

Pending imports now hold the cursor and request a full catch-up, including a fresh
checkpoint even when its advertised frontier appears contained. Recovery includes
the device's own rows and uses the existing reconnect backoff. Overlapping HTTP and
WebSocket catch-ups cannot clear a newer repair request. Incomplete checkpoints are
rejected before persistence. The HTTP fallback also advances across a contained
checkpoint's trimmed prefix before applying its remaining rows.

Tests cover WebSocket repair, HTTP repair, the contained-checkpoint boundary, and
actual Loro/SQLite persistence followed by a restart. These fixes cover identified
failure modes; a server that lacks the necessary history or an unavailable model
provider can still prevent a turn from completing.

## Resource changes

- Unchanged workspace lists no longer wake their subscribers. The existing
  periodic device tick remains, so time-based presence expiry still updates.
- Pure text/reasoning appends notify the affected transcript. Structural changes,
  pending-send acknowledgements, tool changes and errors still notify app state.
- Sidebar rows and small loaders retain their rendered views. Changes to row
  properties, navigation, hover and settings invalidate them normally.
- Metal windows use two drawable buffers instead of three. This removes a
  full-resolution surface from the pool; it does not change shader quality or
  remove a blur effect. See Apple's [drawable pool documentation](https://developer.apple.com/documentation/quartzcore/cametallayer/maximumdrawablecount).

The buffer experiment used the same instrumented executable with either pool
size. Two buffers saved about 29 MiB in the populated streaming test. Steady-state
95th-percentile acquisition waits rose from roughly 0.03 ms to 2.4–2.7 ms, while
95th-percentile acquisition plus encoding remained below 3 ms. The measured
transcript render cadence remained about 42 renders/s in both cases. These are
render timings for this workload, not a guarantee about every display or GPU.

## Idle memory interpretation

An approximately one-minute sample of the installed v0.2.39 app reproduced physical
footprint changes from 186 to 416 MiB. Tracked malloc-zone allocations stayed near 11 MiB; paired
footprint reports attributed most of the difference to owned, unmapped graphics
memory, about 11 versus 221 MiB. Most main-thread samples were asleep.

The malloc-zone figure is not a complete inventory of Rust allocator memory.
In the candidate's 371 MiB sample, `vmmap` reported about 222 MiB of owned
unmapped memory, 75 MiB of dirty/swapped IOAccelerator memory and 36 MiB of
IOSurface memory. The paired installed-build footprint reports identify the large
changing unmapped category as graphics memory. Its individual allocation owners
have not yet been established.

That evidence does not show a 230 MiB Rust heap leak. Nor does it identify every
driver/compositor allocation responsible for the changing charge. The drawable
reduction lowers measured graphics memory, but does not establish that macOS will
stop changing the app's reported footprint. Virtual address-space totals in a
crash report are not physical memory usage.

A later 65-second foreground sample of the candidate using the real synced profile
averaged 1.40% CPU and stayed between 362 and 372 MiB. The earlier installed-build
sample averaged 1.88% CPU, but its median footprint was lower (199 versus 371 MiB).
These diagnostic samples were not a controlled pair with identical focus and
graphics residency. They do not establish an across-the-board idle memory win.

## Native measurements and validation

M4 Pro, 24 GiB, macOS 26.0.1, 1320×880-point foreground window at 2× scale,
50 additional idle conversations. Normal-rate values average two runs per build;
the faster workload has one run per build. Binaries were built in release mode
with symbols retained. No build or test ran alongside these measurements.

| Streaming metric | v0.2.39 normal | Candidate normal | v0.2.39 fast | Candidate fast |
| --- | ---: | ---: | ---: | ---: |
| UI average CPU, % of one core | 18.24 | 16.91 | 19.04 | 18.29 |
| Engine average CPU, % of one core | 0.61 | 0.61 | 0.87 | 0.88 |
| UI peak physical footprint, MiB | 330.42 | 307.21 | 334.24 | 311.27 |
| Engine peak physical footprint, MiB | 28.14 | 25.50 | 28.55 | 30.70 |
| UI peak two-second CPU, % of one core | 20.79 | 18.81 | 19.89 | 19.00 |

The normal-rate UI CPU reduction is about 7%; the faster workload's is about 4%.
UI peak footprint falls about 23 MiB in each. Engine memory varies between runs;
its faster-workload peak increased about 2 MiB. This is a modest resource gain
alongside concrete stability fixes, not evidence of universal performance limits.
The total app cost includes both processes. Their separate memory peaks need not
occur at the same instant.

All completed replies have identical hashes within each workload. The public
[per-run summaries](performance/macos-stability.json) retain executable hashes,
phase durations, both processes' measurements and reply hashes. Native replay
results must not be extrapolated to every provider's chunking, cloud profile,
background workload or long-running session.

Validation also passed:

- 32 sync, 129 engine and 596 UI library tests. Git fixture tests ran with global
  URL rewrites disabled (`GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1`).
- 208 GPUI and 18 macOS renderer library tests, including blur pixel checks.
- Eight consecutive short replies and three consecutive long replies through
  native composer input, without the reproduced abort or a stuck turn. These
  functional runs overlapped compilation and are excluded from the resource table.
- Eight cached-versus-fresh visual checks with 50 background conversations:
  settled, sidebar hidden/restored, scrolled, selected, typed, model menu and menu
  dismissed. The menu comparison excludes only its animated loading region.

A live-provider check returned explicit authentication errors because the local
Claude CLI needs sign-in again. It is excluded from successful-turn validation.
The profiling driver now rejects error parts even when the containing transcript
entry has settled to `complete`. A final focus-transition diagnostic lost
foreground focus and was discarded, rather than counted as a resource result.

## Reproduction

Build with `cargo build --release --locked -p zeron`, and copy each binary to an
immutable path before profiling. Use an unlocked, awake display with no concurrent
build or test workload. The native helper sizes the foreground window to 1320×880
points and rejects focus loss. Default dark appearance and animations are enabled.

```sh
CLAUDE_CODE_EXECUTABLE="$PWD/scripts/replay-claude.py" \
ZERON_REPLAY_JOURNAL="$PWD/scripts/fixtures/resource-stream.jsonl" \
ZERON_PROFILE_BACKGROUND_CHATS=50 ZERON_PROFILE_SUBMIT_UI=1 \
ZERON_PROFILE_PROMPT='Replay fixture.' \
node scripts/resource-profile.mjs /path/to/zeron /tmp/fresh-profile claude-code
```

The fixture emits 52,624 combined text/reasoning bytes with a 40 ms delta delay.
For faster streaming, set `ZERON_REPLAY_REPEAT=4 ZERON_REPLAY_DELAY_MS=10`.
For the crash regression, select `runway-short-stream.jsonl`, set the delay to
400 ms and `ZERON_PROFILE_TURNS=8`. Submission uses actual native composer key
events and verifies the exact prompt and successful completion of every turn.

The profiler measures UI and engine separately, sampling native CPU time and
physical footprint every 500 ms. CPU is percent of one core. Phase averages must
not be compared directly with an instantaneous Activity Monitor peak. Replays use
the real engine, RPC and CLI adapter, but make no model API calls and do not
simulate every cloud/network failure.
