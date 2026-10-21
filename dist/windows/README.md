# Windows icon

`zeron.ico` contains 16, 24, 32, 48, 64, 128, and 256 pixel PNG frames
generated alongside `../macos/icon-1024.png` from `../zeron.svg` by
`python3 scripts/gen-brand-icons.py` (Pillow required). It preserves the black
Crew tile's transparent exterior.

`apps/zeron/build.rs` compiles `zeron.rc` into the Windows executable for
both debug and release builds. Resource ID 1 is required by GPUI's Windows
icon loader. No adjacent image file is needed at runtime; the installer
(`zeron.iss`) uses the same `zeron.ico` for its own window and the uninstall
entry.
