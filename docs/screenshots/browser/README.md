These are real application captures from the opt-in `browser-fixture` example,
which runs Zeron's shell with synthetic local chat data and a loopback website.
The fixture does not launch an agent or connect to an engine.

The Linux captures show the explicitly labeled external-browser fallback.
Native macOS captures come from the `browser-macos-captures` CI artifact.

Regenerate on macOS with:

```sh
cargo run --release --locked -p zeron-ui --example browser-fixture \
  --features browser-fixture -- /tmp/browser-captures
```

Linux requires an X11 desktop session, `xdotool`, and ImageMagick. These screenshots
are not included in the app's embedded assets or distribution bundle.

The committed macOS images were captured by [CI run 34290571239](https://github.com/zeronsh/comet/actions/runs/34290571239)
at source commit `29eaf99e`. Its native fixture completion marker and all checks
passed. The hosted Mac has a 1024px desktop, so the fixture uses Zeron's existing
collapsed-left-sidebar layout. The Linux captures use a 1320px window with both
sidebars visible.
