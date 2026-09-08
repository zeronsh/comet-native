# Composer performance validation — PR #271

The audit found and fixed an unbounded repaint loop in the earlier PR. Intrinsic
layout measured the editor at a provisional 320px width and then its resolved
width. Notifying the composer for both heights scheduled another layout forever
once wrapping differed. With a 1,000-line draft, unfocused CPU stayed at 238% of
one core before the fix. It now returns to 0.2%, within the baseline's 0–0.2%
range. Notifications now originate in prepaint, only when final geometry changes.

The editor also retains one shaping key alongside its existing wrapped lines.
Height, scrolling, selection, and caret blinking can reuse that layout. Text
edits, width, font/size, colors, placeholder, mention mode, and IME marked ranges
invalidate it. The cache does not retain draft history. Maximum glyph ascent is
computed with layout rather than rescanned on every render. Final prepaint
ensures the retained layout matches the actual viewport width.

## Release measurements

UI-process CPU, including rendering workers; percent of one core:

| Phase | Base main | Earlier PR | Fixed PR |
| --- | ---: | ---: | ---: |
| Empty, focused | 9.00 | 8.60 | 9.10 |
| Typing / growing | 167.39 | 211.78 | 201.63 |
| Normal scrolling | 29.99 | 233.47 | 29.95 |
| Paste / resize | 218.46 | 204.94 | 213.48 |
| 1,000-line paste | 228.66 | 230.43 | 230.32 |
| 1,000-line scrolling | 62.39 | 239.16 | 62.02 |
| Large draft, focused idle | 15.50 | 241.39 | 16.80 |
| Unfocused idle | 0.10 | 237.99 | 0.20 |

The fixed PR's average peak RSS was **212.37 MiB**, versus **212.74 MiB** for
main and **212.94 MiB** for the earlier PR. Large-draft scrolling main-thread
CPU fell from 3.36% on main to 2.48% with the fix.

This is not a claim of zero animation overhead. During typing, main-thread CPU
was 15.83%, versus 14.77% on main (+1.06 percentage points). Total process CPU
was about 20% higher during that phase, which includes rendering the additional
animation frames. That work is bounded by the 180ms animation and ends when it
settles. Focused idle includes caret blinking. The unfocused samples are near
`/proc` counter resolution; a 0.1-point difference is not meaningful.

## Method and reproduction

[Raw runs, executable hashes, and summaries](performance/composer-layout.json)
compare `f9fbca6` (the PR's base main), `28e1537` (earlier PR), and the fixed
candidate identified by source and executable SHA-256. Main and the fixed PR
were each run twice; the earlier PR diagnostic was run once. Runs used release
builds, immutable executable copies, fresh UI data directories, the same isolated
mock-engine fixture, a dedicated 1280×900 Xvfb window, and `LP_NUM_THREADS=4`.
Compilation was finished before each measurement. Linux x86_64, Rust 1.97.1.

[The profiler](../scripts/profile-composer.py) drives real key, clipboard, and
wheel events without submitting a turn. It samples RSS every 100ms and reads
process and main-thread CPU counters at phase boundaries. The fixture has two
seeded chats in its sidebar and no streaming activity. For each binary, use a
fresh output directory on the dedicated display:

```sh
python3 scripts/profile-composer.py /path/to/zeron /tmp/composer-run-1 \
  --settings /path/to/mock-ui/ui-settings.json \
  --ipc-port 27997 --display :117
```

The sequence covers focused idle, sixteen typed lines, bidirectional scrolling,
six alternating three/nine-line pastes, three 1,000-line pastes, long-draft
scrolling, and focused/unfocused idle. The output includes a screenshot to check
that the intended draft and window were exercised. Measurements exclude the
mock daemon and input-driving processes. They are comparative Linux/Xvfb
measurements, not macOS GPU, frame-latency, or battery-life measurements.

## Regression coverage

All **627 UI tests** pass. Two added Linux headless tests exercise the real editor:

- 120 layout passes with changed viewport heights, scroll positions, and selection
  reuse one shaped layout; relevant text, styling, placeholder, mention, and IME
  changes invalidate it.
- After publishing final geometry, 30 actual window draws emit no additional
  layout notifications. This guards the provisional-measurement repaint loop.

Existing resize, row-reveal, overflow, editing, transcript, and picker tests remain
in the full suite. Validation commands:

```sh
cargo build --release --locked -p zeron
cargo test --locked -p zeron-ui --lib -- --test-threads=1
```
