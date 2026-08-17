# Packaging

App icons are sourced from `assets/brand`:

| Target | Source |
| --- | --- |
| macOS `.icns` | `assets/brand/png/crew-icon-macos-1024.png` |
| Linux hicolor | `assets/brand/png/crew-icon-1024.png` and `assets/brand/crew-icon.svg` |
| iOS AppIcon | `assets/brand/png/crew-icon-1024.png` copied to `Assets.xcassets/AppIcon.appiconset/AppIcon1024.png` |

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/comet-<version>-linux-<arch>.tar.gz` containing:

- `comet` — the binary (headed by default; `comet headless` runs the engine alone)
- `comet.desktop` — XDG desktop entry
- `comet.png` / `comet.svg` — raster and scalable Crew app icons
- `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`,
  including both the `1024x1024` and scalable hicolor icon paths

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/comet-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Crew.app` (Info.plist + icns), ad-hoc
signs it (set `CODESIGN_IDENTITY` for a real Developer ID), and wraps it in a
dmg. CI runs this on tags (`.github/workflows/release.yml`). The manual steps
it automates, for reference (run on a macOS host — gpui needs Metal; no
cross-build from Linux):

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
   mkdir -p Crew.app/Contents/{MacOS,Resources}
   cp comet Crew.app/Contents/MacOS/comet
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Crew.app/Contents/Info.plist
   ```
3. Icon: generate `comet.icns` from the squircle Crew raster and place it at
   `Crew.app/Contents/Resources/comet.icns`:
   ```sh
   mkdir comet.iconset
   sips -z 256 256 assets/brand/png/crew-icon-macos-1024.png \
     --out comet.iconset/icon_256x256.png
   iconutil -c icns comet.iconset -o Crew.app/Contents/Resources/comet.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Crew.app
   xcrun notarytool submit Crew.zip --keychain-profile … --wait
   xcrun stapler staple Crew.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Crew -srcfolder Crew.app -ov -format UDZO Crew.dmg`).
