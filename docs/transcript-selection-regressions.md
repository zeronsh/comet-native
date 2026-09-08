# Streaming selection and prompt expansion

Text selection now releases automatic viewport following at mouse-down and
materializes the current list position. A stream burst can no longer remove
the drag anchor before the first mouse move. Selection retains the runway;
edge scrolling and returning to the live tail use the existing navigation paths.

Show more adds the prompt's revealed height to the reservation, using the
prompt's resize curve. Show less reverses it. Expanding the prompt therefore
does not count as assistant output or permanently retire the runway. Real
output still consumes the reservation, including when the prompt is expanded.

## Native recordings

- [Show more, Show less, and retained runway during streaming](https://github.com/user-attachments/assets/55162a94-8b6b-4725-9bfd-e67b173799c8)
  (43 seconds): expand and collapse the prompt, apply downward wheel input,
  select live text, and continue streaming with the reserved space intact.
- [Select an overflowing live reply, then resume scrolling](https://github.com/user-attachments/assets/aa4c50dc-a7c9-4b84-a02b-63da3809e877)
  (38 seconds): hold a paragraph selection while more code arrives, release,
  jump to the live tail, and scroll up and down during the same turn.

These are native Linux/X11 recordings at 1280×800 using Xvfb and software
Vulkan, with four renderer workers. Both runs submitted through the composer
and replayed deterministic Claude adapter events without model API calls.
The long replay was briefly paused to position the pointer, then resumed at
mouse-down before the first drag movement, exercising the burst regression.
Both replay runs completed successfully. These are functional recordings,
not performance benchmarks or native macOS validation.

The recorded release binary has SHA-256
`d304b603cc56575742d05e1b09fc722e74e884ffdec191abac220f3f81990e5c`.

## Automated validation

All 620 release UI tests passed:

```sh
cargo test --release --locked -p zeron-ui --lib -- --test-threads=1
```

The new headless tests use the cached transcript view and real mouse events.
They cover a burst between mouse-down and the first drag movement, selection
across streaming and completion, and expanding/collapsing a live prompt before
jumping to enough output to retire the reservation. The burst and Show more
regressions both failed before their respective fixes.

The existing suite also covers runway geometry, wheel escape and re-sticking,
resizes, completion shrinkage, viewport restoration, and selection edge scrolling.
The native app also built with `cargo build --release --locked -p zeron`.

Replay with `scripts/resource-profile.mjs` and `scripts/replay-claude.py`:
use `scripts/fixtures/transcript-selection-stream.jsonl` at 1200 ms per delta
for the short turn, or `scripts/fixtures/resource-stream.jsonl` at 300 ms per
delta for overflow. Set `ZERON_PROFILE_SUBMIT_UI=1`; the short-turn prompt
must be long enough to expose Show more. The recordings used a 1939-character
prompt for that case.
