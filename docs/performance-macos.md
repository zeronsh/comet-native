# Native macOS resource follow-up

For the subsequent v0.2.39 native scrolling crash, stuck-send recovery and
populated-chat measurements, see the [stability follow-up](performance-macos-stability.md).
The measurements below predate those regressions and do not establish stability
for consecutive native composer sends.

This follows PR #255 at `f10ef38c`. The app pins zui `0003b923`, which gates
unnecessary main-queue frame callbacks and bounds native Metal blur resources.
The Linux software-Vulkan results in [the earlier report](performance-resource-usage.md)
do not predict native Metal usage.

## Changes

- Transcript row derivation now has a content revision. Presence, catalog and
  other unrelated state notifications skip rebuilding rows, while selection,
  replay readiness, optimistic echo insertion/removal, transcript deltas and
  subagent content invalidate the cache. Live entries also skip an unused
  serialized fingerprint.
- The primary transcript reuses its GPUI scene on unrelated sidebar animation
  and caret frames. Transcript updates, explicit shell notifications, scrolling,
  geometry changes and full refreshes still invalidate it. This retains live
  session status and elapsed labels while avoiding redundant layout and paint.
- The native frame loop parks clean-window callbacks and wakes for content,
  input or animations. It keeps the existing display clock and lifecycle
  protections. The timing thread still runs while the window is subscribed.
- Metal uses a per-frame autorelease pool and bounded caches for alternating
  blur surfaces. Smaller texture buckets reduce over-allocation; window shrink
  and idle transitions release obsolete extents. Current surfaces retain their
  resources. Blur sigma, shaders, clipping, animation clocks and scroll behavior
  stay unchanged.
- The resource driver now reports native process CPU and physical footprint as
  well as RSS. Its window helper rejects locked/asleep displays and invalidates
  measurements if the app loses the foreground or its window disappears.
- Replay recording clones frames before the transcript applier mutates them.
  Otherwise later appends corrupt historical reset/upsert records.

## Validation and limits

593 UI tests, 207 GPUI library tests and 15 native macOS tests passed. The native
suite covers wake after idle, animation continuation, resize, Gaussian blur
pixels, identical cold/warm rendering, cache bounds and menu-close trimming.
The macOS pasteboard tests share the system clipboard and are run serially.
The optional native UI replay example uses CoreText, Metal, the real shell and
real animation clocks; its completed transcript is rendered to `complete.png`.
The profiling feature and its benchmark dependencies are not enabled in normal
application builds.

The final completed native replay image is pixel-identical to the baseline.
With `ZERON_VERIFY_CACHE=1`, the example also compares the reused scene with a
forced fresh render after settling, hiding/restoring the sidebar and scrolling;
all four comparisons are pixel-identical. With the bundled 80-section fixture,
`ZERON_VERIFY_INTERACTIONS=1` also drives a text-selection drag, composer typing,
and model-menu open/outside-click dismissal. Selection, typed text and dismissal match a fresh
render byte for byte. The model menu has no engine catalog in this isolated
example, so its loading bars keep animating: the comparison excludes the menu
rectangle, and both complete images are retained for visual review. This checks
opening/dismissing the popover, not selecting a live provider model.

A precompiled-shader experiment also produced identical pixels but no meaningful
foreground memory improvement, so the existing runtime-shader build
configuration is retained.

## Native foreground measurements

The display was subsequently unlocked for native-window replay. One valid run
of the earlier PR head (`f10ef38c`) and two of the final application changes used
the same sanitized 80-section Rust reply, real engine/Claude adapter/RPC, default
dark appearance, and 1320×880 logical window at 2× scale. Builds and sampling
profilers were stopped during measurement. The helper verified foreground
activation and window visibility throughout every retained run. Interrupted
runs and runs with diagnostic sampling are excluded.
[Raw counters, executable hashes and source identity](performance/resource-macos-foreground.json).

| Native metric | Earlier PR head | Updated, two runs |
| --- | ---: | ---: |
| Empty-chat UI CPU | 1.25% | 0.59–0.61% |
| Streaming UI CPU | 15.16% | 12.48–13.31% |
| Streaming UI + engine CPU | 16.06% | 13.26–14.09% |
| Empty-chat peak UI physical footprint | 306.38 MiB | 302.11–303.50 MiB |
| Streaming peak UI physical footprint | 328.84 MiB | 323.11–325.99 MiB |

Mean streaming UI CPU is 12.90%, about 15% below the earlier PR head in this
workload. The native memory reduction is modest. The final 10-second settled
observations averaged 0.83–0.87% UI CPU and ended at 323.31–326.25 MiB; the
baseline used a longer 30-second observation, so those settled averages are
not a matched-duration before/after claim. All completed replies have the same
52,624-byte combined text/reasoning digest.

A larger replay repeated the fixture four times at a 10 ms provider delay,
producing 210,502 combined text/reasoning bytes in about 21.5 seconds. Two valid
foreground runs used 13.03–15.34% streaming UI CPU and peaked at
328.74–329.75 MiB UI physical footprint. The first ten seconds after completion
included additional work (3.50% UI CPU in the first run); the second run's final
ten seconds of a 30-second observation averaged 0.80% and ended at 332.41 MiB.
This checks a larger, faster reply without establishing long-duration memory
stability or a matched baseline improvement.
[Stress-run counters and executable identity](performance/resource-macos-stress.json).

CPU uses 100% per core. Physical footprint includes memory charged by macOS,
including compressed and GPU allocations; RSS alone cannot represent this.
These are short replay measurements on one Apple Silicon Mac, not a guarantee
for every model, transcript length, display or interaction. They do not prove
that every reported idle memory swing is fixed. Native foreground rendering
and replay completion passed; typing/selection across multiple panes, display
reconnection and long-duration mixed-use coverage remain limited. Existing
animation timing, blur sigma, shaders and clipping are unchanged.

## Earlier isolated native offscreen comparison

Two sequential runs per build, ordered baseline/candidate/candidate/baseline,
on macOS 26.0.1 (25A362), arm64 Mac16,7 with 24 GiB RAM. The synthetic shell
replays the same protocol frames at the recorded cadence, at 1320×880 logical
pixels and 2× scale, then waits 15 seconds. Each phase excludes its first two
seconds. Both drivers use the same allocator and native text/Metal backends.
The baseline adds only the profiling driver and embedded-platform launch support.
[Raw samples and executable/trace hashes](performance/resource-macos-offscreen.json).

| UI-only replay metric | PR #255 | Candidate |
| --- | ---: | ---: |
| CPU, 100% per core | 18.49% | 18.46% |
| Mean of per-run peak physical footprint | 282.85 MiB | 286.69 MiB |

These earlier runs, before the primary transcript scene-cache change, establish
**no meaningful streaming CPU or memory improvement** in the isolated workload. It excludes native display-link dispatch and desktop
composition and does not open alternating menus. The completed 2640×1760 app
image is byte-identical between baseline and candidate in the first matched pair.
Keep this historical result separate from the later native-window measurements
above; its polling loop and compositor coverage differ. It is not a universal
memory floor.

## Reproduction

Requires Node 22+, Rust and Xcode Command Line Tools. Foreground profiling needs
existing Accessibility access to activate and size the isolated window. Keep
that window foreground and the display awake for the entire run. Replay uses
an isolated profile and the bundled sanitized fixture; it makes no model API call.

```sh
cargo build --release --locked -p zeron
ZERON_FRAME_STATS=0 ZERON_PROFILE_IDLE_MS=10000 \
  CLAUDE_CODE_EXECUTABLE="$PWD/scripts/replay-claude.py" \
  ZERON_REPLAY_JOURNAL="$PWD/scripts/fixtures/resource-stream.jsonl" \
  node scripts/resource-profile.mjs target/release/zeron /tmp/zeron-native claude-code

# UI-only offscreen replay of those verified protocol frames:
cargo build --release --locked -p zeron-ui --features resource-profile \
  --example macos-resource-profile
ZERON_VERIFY_CACHE=1 ZERON_VERIFY_INTERACTIONS=1 ZERON_FRAME_STATS=0 \
  target/release/examples/macos-resource-profile \
  /tmp/zeron-native/frames.json /tmp/zeron-native-ui

# Larger/faster reply: use the foreground command above with these added:
# ZERON_REPLAY_REPEAT=4 ZERON_REPLAY_DELAY_MS=10 ZERON_PROFILE_IDLE_MS=30000
# and a fresh output directory.

# Native counter usable with either process (CPU uses 100% per core):
xcrun clang -O2 scripts/macos-resource-stat.c -o /tmp/zeron-stat
/tmp/zeron-stat PID
```

Compare fresh release builds in alternating order, with the same trace,
window geometry, display scale and settings. Do not compile or run a profiler
concurrently with timed comparisons. Engine and harness processes are reported
separately from the UI; the offscreen example excludes both and the window
compositor. Its polling loop must not be reported as native idle CPU.
