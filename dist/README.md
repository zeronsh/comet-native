# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/comet-<version>-linux-<arch>.tar.gz` containing:

- `comet` — the binary (headed by default; `comet headless` runs the engine alone)
- `comet.desktop` — XDG desktop entry
- `comet.png` — 256×256 placeholder icon (replace before shipping)
- `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS (config only — not yet executed)

The `dist/macos/Info.plist` template is ready; the bundling steps, to be run on a
macOS host (gpui needs Metal; no cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p comet --target aarch64-apple-darwin
   cargo build --release -p comet --target x86_64-apple-darwin
   lipo -create -output comet \
     target/aarch64-apple-darwin/release/comet \
     target/x86_64-apple-darwin/release/comet
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p Comet.app/Contents/{MacOS,Resources}
   cp comet Comet.app/Contents/MacOS/comet
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Comet.app/Contents/Info.plist
   ```
3. Icon: generate `comet.icns` from `dist/comet.png` (`iconutil`) and place it at
   `Comet.app/Contents/Resources/comet.icns`:
   ```sh
   mkdir comet.iconset && sips -z 256 256 dist/comet.png --out comet.iconset/icon_256x256.png
   iconutil -c icns comet.iconset -o Comet.app/Contents/Resources/comet.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Comet.app
   xcrun notarytool submit Comet.zip --keychain-profile … --wait
   xcrun stapler staple Comet.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Comet -srcfolder Comet.app -ov -format UDZO Comet.dmg`).
