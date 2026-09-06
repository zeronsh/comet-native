# Runway layout regression and performance validation — PR #261

The layout fix showed no material CPU or memory regression in eight matched
Linux replays against v0.2.41 (`489a8c2`). The tested candidate is `d8c5760`,
including that same sidebar fix. Streaming CPU changed by −0.26% on long replies
and +0.48% on short replies. Peak RSS differed by less than 0.8 MiB in both cases.
The short-stream UI main-thread difference was +0.29 percentage points; this
comparison does not claim literally zero additional work or a universal speedup.

The GPUI dependency changes from `36ca993` to `8425850`, whose only change is
virtual-list reservation sizing. Renderer, scene caching, shared snapshots,
markdown caches, and spring parking optimizations are preserved. Reservation
layout visits the existing measured window and queries the height tree; it does
not render the full transcript or add an idle animation loop.

| Workload | Metric | v0.2.41 | Candidate |
| --- | --- | ---: | ---: |
| Long | Streaming CPU (%) | 136.19 | 135.83 |
| Long | UI main-thread CPU (%) | 10.25 | 10.25 |
| Long | Peak RSS (MiB) | 220.73 | 221.47 |
| Long | Peak PSS (MiB) | 163.33 | 164.08 |
| Long | Settled CPU, last 10 s (%) | 7.78 | 7.78 |
| Short | Streaming CPU (%) | 95.83 | 96.30 |
| Short | UI main-thread CPU (%) | 7.28 | 7.57 |
| Short | Peak RSS (MiB) | 209.53 | 210.32 |
| Short | Peak PSS (MiB) | 152.75 | 153.29 |
| Short | Settled CPU, last 10 s (%) | 5.73 | 5.68 |

Values average two runs per build, in main/candidate/candidate/main order for
each workload. CPU is percent of one core. Memory sums UI and engine phase peaks,
which may occur at different instants. All runs completed with identical reply
hashes within each workload. The final ten settled seconds show no sustained
idle CPU increase. Individual summaries and executable/source identities are in
[the raw results](performance/runway-layout.json).

## Functional regression coverage

All **611 release-mode UI tests** passed, including ten new headless GPUI layout
and handler tests. The initial new tests failed on v0.2.40/main: the short-chat
animation jumped from 660 px to 0 px in one frame, and a three-row append increased
the scroll range from 2 px to 230 px before the corrective frame.

The new coverage checks:

- Smooth, monotonic short-chat glides and first echoes arriving after send.
- First-paint geometry when multiple rows consume a runway.
- Real transcript streaming through overflow, tail-following, and completion.
- Real wheel events at the end, without entering a gap or reversing direction.
- Background commits without frames, followed by downward-only navigation.
- Second sends and steering while preserving the viewport until echo arrival.
- User scroll-up ownership while new output outgrows the reservation.
- Viewport resizing in the same layout, without a provisional gap.
- Direct navigation to a virtualized tail with unknown preceding row heights.

Existing regressions also cover same-layout completion shrinkage, remeasurement
during glides, off-screen overflow retirement, session restoration, and stale
hold cancellation. The UI suite now runs in `.github/workflows/ui-tests.yml` for
relevant pull requests and main updates.

## Native recordings and measurement scope

The PR has two GitHub user-attachment videos from a native Linux/X11 window:
first send → runway → overflow following → completion → repeated downward input;
and second send → scroll up → focus another window during streaming → refocus
and use only downward input. They were captured before integrating the unrelated
sidebar change; the scrolling code and GPUI list revision are identical to the
final candidate. All tests and timed measurements above include the sidebar fix.

Benchmarks used a dedicated Xvfb display, a 1280×800 foreground window, software
Vulkan with four render workers, fresh local profiles, and composer submission.
Compilation and recording were stopped during timed replays. This is a matched
Linux comparison, not a native macOS/Metal benchmark or long-duration stability
claim. Normal shared-host noise limits interpretation of small differences.

## Reproduction

Build each revision with `cargo build --release --locked -p zeron`, copy each
binary to an immutable path, and run `scripts/resource-profile.mjs` sequentially
in main/candidate/candidate/main order for each workload. Use:

```sh
DISPLAY=:108 WAYLAND_DISPLAY= LP_NUM_THREADS=4 ZERON_FRAME_STATS=0 \
  ZERON_PROFILE_PSS=1 ZERON_PROFILE_SUBMIT_UI=1 \
  ZERON_PROFILE_PRE_IDLE_MS=10000 ZERON_PROFILE_IDLE_MS=45000 \
  ZERON_REPLAY_DELAY_MS=40 \
  CLAUDE_CODE_EXECUTABLE="$PWD/scripts/replay-claude.py" \
  ZERON_REPLAY_JOURNAL="$PWD/scripts/fixtures/resource-stream.jsonl" \
  node scripts/resource-profile.mjs /path/to/binary /tmp/fresh-run claude-code
```

The long fixture contains 52,624 text/reasoning bytes. For the short retained
runway, use `scripts/fixtures/runway-short-stream.jsonl`, a 400 ms delta delay,
5 s pre-idle, and 20 s settled time. No model API calls are made.
