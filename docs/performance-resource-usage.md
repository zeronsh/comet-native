# Resource profiling, 2026-09-05

For the subsequent native macOS work and its validation limits, see
[Native macOS resource follow-up](performance-macos.md).

Baseline: main `f1d4bad` (v0.2.37). Final application revision: `b09b379`.
The previous optimized build is `cff81e7`, using zui `25f1c948`; the final build
pins [`zeronsh/zui` at `49f9abd19`](https://github.com/zeronsh/zui/commit/49f9abd196705eda6a27080c506295a19b5da63e).
All measured binaries are release builds. The renderer follow-up preserves the
existing animation clocks, scrolling behavior and visual effects.

## Repeated CPU comparison

With four software-rendering workers, streaming CPU fell **40.3%
from old Zeron**, and **33.3% from the previous optimized build**.
The six runs were sequential: old, previous, final, final, previous, old. Each
cell averages two runs. [Raw results](performance/resource-four-workers.json)
include every executable hash, reply hash, phase and individual measurement.

| Metric | Old Zeron | Previous optimized | Final |
| --- | ---: | ---: | ---: |
| Idle CPU | 27.41% | 15.75% | 7.49% |
| Streaming CPU | 265.57% | 237.60% | 158.57% |
| Post-completion CPU, last 10 s | 24.34% | 17.13% | 9.14% |
| Idle peak RSS | 330.92 MiB | 204.24 MiB | 206.11 MiB |
| Streaming peak RSS | 358.73 MiB | 229.64 MiB | 235.24 MiB |
| Streaming peak proportional memory | 301.38 MiB | 172.07 MiB | 177.19 MiB |

The new pipelines add about 1.9 MiB of idle RSS and
5.6 MiB of streaming RSS compared with the previous optimized build.
This is a small memory tradeoff for the CPU reduction, with no new image or scene
cache. Final idle RSS remains 37.7% below old Zeron.

UI + engine totals, foreground 1280×800 window with a focused composer.
The short workload is identical captured Haiku output: 51,769 bytes of Markdown,
or 52,624 bytes including reasoning and part separators, at 40 ms per delta.
CPU uses **100% per core**. These measurements use Linux software Vulkan;
they are not measurements of native Metal or a prediction of laptop CPU percentages.

## One-worker and long-transcript checks

The one-worker short comparison also averages two runs per build:
[baseline](performance/resource-baseline.json),
[final](performance/resource-candidate.json). It uses the same captured output.

| Metric | Old Zeron | Final |
| --- | ---: | ---: |
| Idle CPU | 25.45% | 6.93% |
| Streaming CPU | 113.51% | 100.64% |
| Post-completion CPU, last 10 s | 20.96% | 7.88% |
| Idle peak RSS | 331.10 MiB | 206.64 MiB |
| Streaming peak RSS | 359.39 MiB | 233.21 MiB |

The synthetic long workload repeats the same deltas ten times at 8 ms per delta,
producing a 526,258-byte combined reply. Each build ran it once with one worker:
[baseline](performance/resource-long-baseline.json),
[final](performance/resource-long-candidate.json).

| Metric | Old Zeron | Final |
| --- | ---: | ---: |
| Streaming CPU | 112.30% | 103.13% |
| Post-completion CPU, last 10 s | 92.77% | 8.30% |
| Streaming peak RSS | 398.13 MiB | 256.27 MiB |

A single graphics worker can constrain streaming throughput; the repeated
four-worker runs are the primary CPU comparison. The long completion check also
covers the earlier spring defect: completed replies could continue requesting
frames and restart their glide on virtual-list height estimates. The final build
parks and anchors to the final list item while retaining real spring motion.

## Final renderer changes

- Plain rectangular fills use a small fragment shader that avoids the general
  quad shader's per-pixel instance reads and control flow.
- Fully opaque fills skip destination blending and unnecessary clipping only
  when coverage is proven. The eligibility check rounds geometry outward so
  rasterization near fractional pixel edges cannot bypass a required clip.
- On software adapters, large solid shapes use the fast fill in a conservative
  interior rectangle. Four disjoint exterior strips retain the original shader
  for corners, borders and fades. Small shapes, gradients, non-finite values and
  coordinates beyond the precision guard keep the general path.
- Physical GPUs keep the original batch structure. They use specialized shaders
  only for homogeneous batches, avoiding the extra draw-call overhead of the
  software-specific split.

Diagnostic category omissions identified quads as the largest remaining cost:
removing quads temporarily reduced streaming CPU from about 234% to 95%; removing
text, paths or shadows barely changed it. Those incomplete diagnostic frames are
not shipped and are not performance results for a functioning application.
The full-scene measurements above use the final release binary and complete output.

## Changes supported by profiling

- OpenCode model discovery built a full generic JSON tree for `/provider` and
  deep-copied it before filtering. Heaptrack measured a 112.25 MB engine heap
  peak dominated by this catalog. Discovery and run setup now decode only the
  required IDs, names and reasoning variant keys, skip unused nested bodies,
  and stream the response through a 64 KiB buffer. Cancellation interrupts a
  stalled body read. Connected-provider filtering and older-server fallback
  remain. The catalog is dropped once run setup chooses the variant.
- Engine transcript snapshots are shared across RPC watches. UI row derivation
  borrows app-state messages instead of copying the complete transcript.
- Incremental Markdown trees share immutable completed blocks across canonical,
  display and previous frames. Only the changing tail reparses and mends.
- Tree-sitter queries compile once per used grammar. Injection grammars load
  when actually requested; Markdown no longer eagerly compiles 27 languages.
- Text dissolves use the shared 30 Hz clock; small shell spinners use a 15 Hz
  lease. Leases expire when hidden or retired. Real scroll motion and reduced
  motion behavior remain intact. A stationary spring no longer redraws its
  500 ms state-retention grace period.
- The sidebar has a cached GPUI view identity, invalidated by shell changes
  and its own animations. Markdown flatten/code caches retain the viewport
  and overdraw rather than every row visited while scrolling.
- The wgpu blur prepares normalized Gaussian weights once per surface, pairs
  adjacent unit-stride texture samples, and scissors each pass to the pixels
  needed by compositing and the next pass. Original downsampled tap positions,
  blur radius and an oversized-kernel fallback remain. Zui's extracted crate
  sources matched the previous fork revision before this patch was applied.

An optimized standalone parser benchmark holds the previous display tree while
constructing the next, using the captured text and 160-byte commits:

| Parser-only metric | Baseline | Optimized |
| --- | ---: | ---: |
| Allocated bytes | 24,268,705 | 8,927,654 |
| Allocation calls | 133,054 | 11,210 |
| Peak live bytes above input | 377,910 | 206,520 |
| Retained bytes above input | 279,770 | 188,057 |
| Elapsed time | 12.84 ms | 2.21 ms |

These are parser-only allocation/time results, not whole-process RSS or CPU.

## Method and limits

Linux x86-64, Xvfb 1440×900, window 1280×800, software Vulkan rendering. CPU sums
all process threads. RSS totals sum each process's phase peak; those peaks need
not occur simultaneously. Proportional memory apportions shared mappings. UI +
engine figures exclude Claude/OpenCode child processes, which are sampled separately.
OpenCode automatic model discovery is included, so some memory savings depend
on its catalog workload. Sampling starts after window configure/focus and does
not capture the complete startup peak.

Every matched run uses a fresh profile, project and chat, 10 seconds before the
prompt, a fixed replay cadence and 45 seconds after completion. The last ten
seconds are reported separately from scroll/fade settling. Runs are sequential,
with no concurrent task builds or manual interaction. Frame diagnostics are off.
The one-worker baseline comes from the earlier pass on this host; the four-worker
comparison reruns all three builds together in forward and reverse order.

The driver snapshots and hashes its executable, validates production transcript
reset/upsert/append/remove frames and requires successful completion. Matching
reply hashes establish identical replay content. Earlier process-targeted `perf`
samples attributed about 95% of UI CPU to software rendering; heaptrack's largest
UI groups were 54.32 MB in lavapipe and 28.38 MB in Gallium/EGL. A final live-run sample attributed 74.01% of UI CPU to JIT shader code and
16.27% to lavapipe, versus 4.23% in the application binary. Remaining CPU on
this host is still predominantly software graphics. These host-specific costs
are not a universal desktop memory floor.

## Validation and reproduction

978 application tests passed during this optimization work, with four existing
ignored tests. The 592 UI tests were rerun in release mode against the final zui
pin. The other coverage includes document (87), engine library (127), harness
library (103), RPC (12), syntax library/integration (25), Claude/OpenCode adapter
integration (21) and targeted engine integration (11).

28 zui renderer/text/atlas tests pass, including the five existing blur numerical
tests and five new fill-path tests. The two graphics tests perform **1,888
byte-identical offscreen image comparisons**: individual shapes and overlapping
batches, opaque and translucent colors, fractional geometry and clipping,
nonuniform and dashed borders, rounded corners, fades, two target formats and
both alpha modes. The real Vulkan tests are explicitly opt-in on machines with
an adapter; they were run here, rather than counted as skipped checks.

A fresh authenticated Haiku 4.5 turn was submitted through the final application's
composer. It completed a shell-tool call and produced 10,902 bytes of
reply text/reasoning. Sidebar, scrolling, text selection/copy, resizing, the model menu and completion
were checked in the real window, reopening the saved transcript for the last checks. Its [summary](performance/resource-live.json) is
functional verification, separate from the fixed-output performance comparison.
Script checks and diff checks passed. This is not a claim that every workspace
integration target ran, or that native Metal performance was measured.

Requires Node 22+, Rust, Python 3 and Xvfb/xdotool for Linux desktop profiling.
The [sanitized fixture](../scripts/fixtures/README.md) reproduces the measured
replay without an API call. Use a dedicated display and fresh output directories;
the driver requires exactly one Zeron window.

```sh
cargo build --release -p zeron
Xvfb :98 -screen 0 1440x900x24 -nolisten tcp
# In another terminal:
DISPLAY=:98 WAYLAND_DISPLAY= LP_NUM_THREADS=4 ZERON_FRAME_STATS=0 \
  ZERON_PROFILE_PSS=1 ZERON_PROFILE_IDLE_MS=45000 \
  CLAUDE_CODE_EXECUTABLE="$PWD/scripts/replay-claude.py" \
  ZERON_REPLAY_JOURNAL="$PWD/scripts/fixtures/resource-stream.jsonl" \
  node scripts/resource-profile.mjs target/release/zeron /tmp/zeron-short claude-code

# Use LP_NUM_THREADS=1 for the one-worker check.
# Add ZERON_REPLAY_REPEAT=10 ZERON_REPLAY_DELAY_MS=8 for the long workload.
# For a live turn, omit replay variables and point CLAUDE_CODE_EXECUTABLE to
# the authenticated CLI; ZERON_PROFILE_SUBMIT_UI=1 submits through the composer.

# In the zui checkout, on a host with Vulkan (software Vulkan works):
LP_NUM_THREADS=4 cargo test --release -p gpui_wgpu --lib -- --include-ignored
```
