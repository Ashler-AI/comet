# Crew brand assets

The mark for Crew (formerly Comet). It follows the Ashler design language documented in
`ashler-platform/DESIGN.md` and the `ui-design-system` / `marketing-design-system` skills.

## The mark

Four ashlar stones laid in bond around a live core.

The stones are offset at the joints, the way a real course of dressed stone is laid — each
stone overlaps the joint below it, and **no single stone closes the square**. That is the
whole idea: a crew is what encloses the work, not any one member. The green core is the
active session at the centre.

It is a sibling to the Ashler mark, not a copy. Ashler is a solid mass with squares cut out
of it; Crew is that same square broken into stones. Both are orthogonal, modular,
monochrome-capable, and built with strong figure/ground.

## Construction

Everything sits on one module grid. Nothing is drawn freehand.

| Unit | Value | Notes |
| --- | --- | --- |
| Stone | 10 | The base module |
| Mortar joint | 2 | The gap between every stone |
| Pitch | 12 | Stone + joint |
| Artboard | 46 | `4 × pitch − joint` |
| Corner radius | 1.2 | 12% of the stone — reads as the 4px `rounded` token at UI scale |

The core is one stone centred on the artboard at `(18, 18)`. Long stones span three pitches
(`34`). Redraw from these numbers rather than scaling a raster.

## Colour

| Token | Value | Use |
| --- | --- | --- |
| `brand-500` | `#09de5e` | The core stone. Nothing else. |
| `neutral-950` | `#0a0a0a` | Ink — stones on light surfaces |
| `neutral-50` | `#fafafa` | Paper — stones on dark surfaces |

Green is an accent, never a fill. Per `DESIGN.md` it is reserved for the active/success
register, which is exactly what the core represents. Do not colour a stone green.

## Files

| File | Use |
| --- | --- |
| `crew-mark.svg` | Default. Stones inherit `currentColor`, core stays brand green — works in light and dark without a second file. |
| `crew-mark-mono.svg` | Single colour throughout, all `currentColor`. Use where green cannot go: stencils, engraving, one-colour print, disabled states. |
| `crew-mark-ink.svg` / `crew-mark-paper.svg` | Pinned to ink or paper when `currentColor` is not available. |
| `crew-lockup.svg` | Horizontal mark + wordmark. Space Grotesk 600, `-0.03em` tracking, outlined — no font dependency. |
| `crew-lockup-mono.svg` | Lockup with the core in `currentColor`. |
| `crew-favicon.svg` | Browser tab. Same geometry, pinned colours. |
| `crew-icon.svg` | App icon on an ink tile at the Ashler `0.10` corner radius. Prefer this everywhere. |
| `crew-icon-light.svg` | Same tile on paper. |
| `crew-icon-macos.svg` | Apple squircle radius (`0.225`), for `.icns` and the Dock **only** — it is deliberately rounder than the Ashler radius so the icon sits correctly next to native apps. |
| `png/crew-icon-*.png` | Raster exports, 16 → 1024. Regenerate from the SVG, do not upscale. |
| `png/crew-icon-macos-1024.png` | Raster of the squircle tile. Feeds the `.icns` in `scripts/package-macos.sh`; committed so packaging does not need an SVG rasterizer. |

## Clear space and minimum size

Clear space on all four sides is **one stone** (`10` units, or 21.7% of the mark's height).
Nothing enters it — no type, no rules, no other marks.

Minimum sizes, verified by rendering rather than assumed:

- Mark: **16px**. The core is still visible at 16px; below that it closes up.
- Lockup: **20px** tall. Under that, use the mark alone.

## Don't

- Don't recolour a stone green or fill the aperture with green.
- Don't add gradients, shadows, glass, or elevation. The design language is flat, border-led
  paper and ink.
- Don't round the stones further. The 4px-equivalent radius is a token, not a preference.
- Don't close the bond into a plain square frame — the offset joints are the mark.
- Don't rebuild the wordmark with live text. Use the outlined lockup so it renders identically
  without Space Grotesk installed.
- Don't set the lockup wordmark in anything but Space Grotesk 600.

## Regenerating

Rasters come from the SVG at full size — never upscale a smaller export. With no
`rsvg-convert` or `inkscape` installed, stock macOS can render the Dock source:

```sh
mkdir -p /tmp/icon
qlmanage -t -s 1024 -o /tmp/icon assets/brand/crew-icon-macos.svg
mv /tmp/icon/crew-icon-macos.svg.png assets/brand/png/crew-icon-macos-1024.png
```

The wordmark is outlined from Space Grotesk 600 (x-height `486`/`1000` em, tracking `-0.03em`).
Every letter in "crew" is an x-height letter, so the wordmark is a clean rectangle with no
ascenders or descenders — it is optically centred on the mark by its x-height band, not by
its bounding box.
