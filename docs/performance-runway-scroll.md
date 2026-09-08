# Runway scrolling performance check — PR #257

No material CPU or memory regression was detected against the newly optimized
`main` (`6d642ac`, PR #255 including #256). The candidate is `ee08569`, the runway
fix merged with that same main. Its only application difference is
`crates/ui/src/transcript.rs`; the optimized renderer pin, transcript scene cache,
shared snapshots, bounded caches and spring parking remain intact.

Eight sequential release replays: main/candidate/candidate/main for each of two
workloads, using fresh profiles and the same executable for each UI/engine pair.
All submissions go through the composer, so they exercise own-send runway logic.
Every run completed successfully, with identical reply hashes within each workload.
The merged code also passed all **602 release UI tests**, including the four new
runway regressions.

| Workload | Metric | Optimized main | Runway fix |
| --- | --- | ---: | ---: |
| Long | Streaming CPU | 134.83 | 135.91 |
| Long | Streaming UI main-thread CPU | 10.20 | 10.40 |
| Long | Streaming peak RSS (MiB) | 221.16 | 221.63 |
| Long | Streaming peak PSS (MiB) | 163.89 | 164.23 |
| Long | Settled CPU, last 10 s | 7.57 | 7.73 |
| Short | Streaming CPU | 95.50 | 95.41 |
| Short | Streaming UI main-thread CPU | 7.42 | 7.49 |
| Short | Streaming peak RSS (MiB) | 209.46 | 210.18 |
| Short | Streaming peak PSS (MiB) | 152.51 | 153.42 |
| Short | Settled CPU, last 10 s | 5.94 | 5.73 |

Values average two runs per build. CPU is percent of **one core** (100% per core).
RSS/PSS sum the UI and engine phase peaks, which may occur at different instants.
The long-reply streaming CPU difference is +0.8%, smaller than the spread between
the two baseline runs; streaming peak RSS differs by less than 0.5 MiB (+0.2%).
There is no sustained post-completion CPU increase from an active runway.
These measurements support retaining the recent performance improvements, rather
than claiming an exact zero cost or a performance improvement from this fix.

## Workloads and scope

- Long: the existing 80-section captured reply, 52,624 text/reasoning bytes,
  40 ms per delta, 10 s before submission and 45 s after completion.
- Short: a synthetic 24-character-chunk reply small enough to retain its runway,
  400 ms per delta, 5 s before submission and 20 s after completion. The last
  10 s specifically checks that the completed hold parks.
- Linux x86_64, Xvfb, 1280×800 foreground window, default light appearance,
  software Vulkan, `LP_NUM_THREADS=4`. No builds or manual interaction ran
  alongside the timed replays. Frame diagnostics were disabled.
- This is a matched Linux comparison, not a new native macOS/Metal benchmark or
  a long-duration memory stability claim. Small differences on this shared host
  should not be interpreted as universal desktop overhead.

[Per-run summaries, executable/source identities and reply hashes](performance/runway-scroll.json)
are retained alongside the fixtures. The profiling driver also writes raw process
samples to each local output directory.

## Reproduction

Build each revision with `cargo build --release --locked -p zeron` and copy the
binaries to distinct paths. Use fresh output directories and a dedicated display.
Run main/candidate/candidate/main sequentially with the following environment:

```sh
Xvfb :108 -screen 0 1440x900x24 -nolisten tcp
DISPLAY=:108 WAYLAND_DISPLAY= LP_NUM_THREADS=4 ZERON_FRAME_STATS=0 \
  ZERON_PROFILE_PSS=1 ZERON_PROFILE_SUBMIT_UI=1 ZERON_PROFILE_IDLE_MS=45000 \
  CLAUDE_CODE_EXECUTABLE="$PWD/scripts/replay-claude.py" \
  ZERON_REPLAY_JOURNAL="$PWD/scripts/fixtures/resource-stream.jsonl" \
  node scripts/resource-profile.mjs /path/to/zeron /tmp/fresh-run claude-code
```

For the short workload, use `scripts/fixtures/runway-short-stream.jsonl`, set
`ZERON_REPLAY_DELAY_MS=400`, `ZERON_PROFILE_PRE_IDLE_MS=5000` and
`ZERON_PROFILE_IDLE_MS=20000`. The replay makes no model API calls.
