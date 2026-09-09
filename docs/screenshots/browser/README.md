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

The committed macOS images were captured by [CI run 34308828299](https://github.com/zeronsh/comet/actions/runs/34308828299)
at source commit `d5c08649`. Its native fixture completion marker and all checks
passed. The hosted Mac has a 1024px desktop, so the fixture uses Zeron's existing
collapsed-left-sidebar layout. The Linux captures use a 1320px window with both
sidebars visible.

The video attached to PR #282 is a continuous 24-second macOS screen recording from the same
fixture run, converted from the artifact’s original MOV to H.264 MP4 without
changing its timing or sequence. It shows rapid tab/toolbar hovering, repeated
tab and toolbar tooltips, and menu dismissal above a live page. The green timer
is rendered by the website using requestAnimationFrame. The native fixture
asserts zero hide/snapshot transitions during hover, preserved tooltip focus,
click-through for passive tooltips, and outside-click isolation/restoration for
the menu. The original MOV and completion marker are retained in the CI artifact.

[Watch the hover regression recording](https://github.com/user-attachments/assets/3b522c2c-6b07-4f74-8c1b-694c27ab8a8c). The video is hosted as a GitHub
user attachment and is not committed to this repository. The screenshots are
documentation only and are excluded from the app bundle.
