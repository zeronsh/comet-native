# Idle presence updates and Metal memory attribution

This pass starts at v0.2.43 (`3e96088`) and uses the signed-in production
backend, including the existing populated history. It follows the unresolved
graphics-memory observation in [the earlier report](performance-macos-stability.md).

## Why the footprint jumps

Paired native `footprint` reports place approximately 210 MiB of the changing
charge in **owned, unmapped graphics memory**. Ordinary malloc categories and
the app's own tracked Metal resources do not grow by that amount. A completed
Metal System Trace recorded 540 identical `currentAllocatedSize` observations
at 46.14 MiB while process physical footprint was approximately 371–382 MiB.
An Allocations launch trace reported 8.63 MiB of persistent heap allocations.
Virtual IOAccelerator mappings in that trace are not physical memory totals.

A standalone reproduction isolates the mechanism without Zeron, a window,
history, network traffic, or streaming. It submits one GPU command every
15 seconds, waits for completion, and samples physical footprint every 500 ms.
On the test M4 Pro with macOS 26.0.1:

| GPU work | Settled footprint | After submission | Transient difference |
| --- | ---: | ---: | ---: |
| Blit only | 6.1 MiB | 94.4 MiB | 88.3 MiB |
| Render only | 31.9 MiB | 154.2 MiB | 122.3 MiB |
| Blit and render | 32.9 MiB | 243.2 MiB | 210.3 MiB |
| Render and MPS Gaussian blur | 50.5 MiB | 274.9 MiB | 224.4 MiB |

The first three reproduce the rise and fall after each of three submissions.
The charge falls approximately 1.5 seconds after GPU completion. Reported Metal
allocation size stays constant for the combined blit/render test at 20.53 MiB.
The additional charge therefore belongs to transient graphics-driver work,
rather than growing application textures or a 210 MiB transcript heap.
The blur test also reproduces the transient charge without an explicit blit.
These sizes and retention times are measurements on this OS/GPU, not Metal API
guarantees or universal limits.

Zeron's rendering uses these command types. Reducing unnecessary submissions
reduces how often the transient charge is incurred. Keeping it resident would
only hold the process at the higher footprint. Streaming and visible animations
still submit GPU work; this change does not promise a flat low footprint while
rendering. The driver observation alone does not identify every smaller change
in the app's memory.

The reusable [standalone probe](../scripts/macos-metal-memory-probe.m) supports
`blit`, `render`, `mixed`, and `blur` workloads:

```sh
clang -fobjc-arc -Wall -Wextra -framework Foundation -framework Metal \
  -framework MetalPerformanceShaders scripts/macos-metal-memory-probe.m \
  -o /tmp/metal-memory-probe
/tmp/metal-memory-probe mixed
```

The checked-in probe was also compiled with warnings enabled and rerun for
80 samples. Its [CSV](performance/metal-driver-probe.csv) records an initial
32.31→242.64→32.31 MiB cycle with 20.28 MiB of Metal allocations throughout
that cycle. Later cycles peak around 234 MiB after smaller caches retire;
they still reproduce the large transient charge. The four-mode table above
records the preceding isolation experiments, rather than identical totals
for every run of the reusable probe.

## Changes

- Device and session heartbeat timestamps still update immediately. An unchanged
  visible presentation no longer invalidates the app. Presence expiry/recovery,
  status, metadata, removals, and device labels still invalidate normally.
- The existing shell timer refreshes relative labels at minute boundaries even
  when presence heartbeats do not redraw.
- The macOS renderer actually stops its display link when frame requests park.
  Invalidation queues a safe restart on the foreground executor, with weak-window,
  closed-window, pending-request, and already-running checks. This avoids recursively
  locking window state from native callbacks.

The renderer change is [zui PR #2](https://github.com/zeronsh/zui/pull/2), pinned at
`8a32c0e26a17b9fab3aa9c2dbb79ed2d28ab2dfe`. Layout, text, blur, shaders, drawable
count, animation timing, scroll behavior, and streaming cadence are unchanged.

## Validation

- 598 UI library tests passed, including new heartbeat presentation, freshness,
  expiry, and recovery regressions.
- 15 native macOS renderer tests passed with runtime shaders.
- The release build completed with the exact published renderer revision and
  no local dependency override.
- The final candidate displayed the production history, completed a native
  composer request with the exact expected reply, scrolled the long transcript,
  and collapsed/restored the sidebar successfully.

The Instruments allocation tracer required `com.apple.security.get-task-allow`
on the temporary profiling bundle. A tiny independent executable reproduced the
attach failure without it and recorded successfully with it. This entitlement
is confined to profiling copies; it is not added to the shipping app.
Completed Allocations and Metal System Trace recordings were retained locally.
Raw traces and screenshots contain private history/environment details and are
excluded from the repository.

## Real-backend measurements

[Measurements, sanitized counters, executable hashes, and run conditions](performance/idle-presence.json)
are retained for review. Process CPU uses 100% for one core; physical footprint includes charged
graphics/compressed memory. The normal desktop process embeds the engine, so
its reported cost includes both UI and local engine. Remote execution cost is
outside this process.

The idle pair uses the same conversation, viewport, 1320×881-point window at
2× scale, and onscreen inactive state. These are not foreground typing results.
No build, test, debugger, or Instruments recording runs during timed comparisons.
Live streaming uses the real remote provider; response length, timing and chunks
can differ, so these runs establish successful operation and observed costs,
not an identical-replay performance guarantee.

| Two-minute idle measurement | v0.2.43 | Candidate |
| --- | ---: | ---: |
| Average CPU, % of one core | 2.114 | 0.749 |
| Peak 500 ms CPU, % of one core | 69.99 | 12.53 |
| Package idle wakeups/second | 8.305 | 0.356 |
| Mean physical footprint, MiB | 202.70 | 171.19 |
| Median physical footprint, MiB | 153.99 | 160.33 |
| Minimum physical footprint, MiB | 151.20 | 148.96 |
| Maximum physical footprint, MiB | 375.36 | 383.24 |
| Samples above 300 MiB | 53 / 240 | 12 / 240 |

In this pair, average idle CPU fell 64.5%, package idle wakeups fell 95.7%,
and mean footprint fell 15.5%. The median increased 6.34 MiB and the maximum
increased 7.88 MiB; the change does **not** establish a lower memory peak or a
universal idle memory floor. Elevated samples fell from 22.1% to 5.0%.
The benefit is less unnecessary work and less time carrying the transient
graphics charge, not elimination of every CPU or memory spike.

| Real-provider streaming observation | v0.2.43 | Candidate |
| --- | ---: | ---: |
| Successful request duration | 93.21 s | 85.70 s |
| Protocol frames observed | 267 | 326 |
| Reply text/reasoning bytes | 13,086 | 13,089 |
| Streaming average CPU, % of one core | 25.59 | 32.59 |
| Streaming peak physical footprint, MiB | 379.45 | 368.31 |
| CPU 10–20 s after completion, % of one core | 1.371 | 0.389 |
| Peak footprint 10–20 s after completion, MiB | 364.86 | 138.89 |

Both runs validated reset/upsert/append consistency, byte lengths, successful
completion, and absence of error parts. Both were observed rendering live text
and a working indicator in the native window. Their independent outputs and
chunk cadence differ: the candidate has a higher observed streaming CPU average.
These live observations do not establish a streaming CPU reduction. The
post-completion windows are short observations; the two-minute idle pair provides
the broader idle comparison.

## Identical-stream regression check

To separate workload cadence from code changes, the repository's native replay
profiler ran the same 52,624-byte fixture at a 40 ms delta delay, with 50 background
conversations, a fresh isolated profile per run, and actual native composer
submission. The helper checked foreground activation and window visibility during
sampling. This supplements the production-backend runs above; replay does not call
a real model provider. Order was baseline, candidate, candidate, baseline.

| Foreground streaming metric, mean of two runs | v0.2.43 | Candidate |
| --- | ---: | ---: |
| UI CPU, % of one core | 17.67 | 17.44 |
| UI + engine CPU, % of one core | 18.38 | 18.16 |
| Mean of UI footprint peaks, MiB | 310.17 | 308.76 |
| Mean of engine footprint peaks, MiB | 27.01 | 28.24 |

All four runs completed successfully with 151 protocol frames and the identical
reply digest `87f610ae8e3be4c4a6eb3ce6655e2f2728e532115fd3b4c5a2db00a0e0ac73fb`.
The candidate UI CPU observations span 16.98–17.91%, so the small mean difference
should be treated as essentially unchanged streaming cost. This check did not
reproduce the large CPU difference in the live-provider samples. It covers this
fixture and machine, not every workload or long-duration interaction.
Separate process memory peaks need not occur at the same instant.
[Per-run summaries and executable hashes](performance/idle-presence-replay.json).

```sh
CLAUDE_CODE_EXECUTABLE="$PWD/scripts/replay-claude.py" \
ZERON_REPLAY_JOURNAL="$PWD/scripts/fixtures/resource-stream.jsonl" \
ZERON_PROFILE_BACKGROUND_CHATS=50 ZERON_PROFILE_SUBMIT_UI=1 \
ZERON_PROFILE_PROMPT='Replay fixture.' ZERON_PROFILE_IDLE_MS=10000 \
ZERON_FRAME_STATS=0 node scripts/resource-profile.mjs \
  /path/to/immutable/zeron /tmp/fresh-replay-profile claude-code
```
