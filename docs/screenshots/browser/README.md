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
