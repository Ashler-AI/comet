#!/usr/bin/env python3
"""Regenerate Crew artwork: python3 scripts/gen-brand-icons.py (requires Pillow).

Source: dist/zeron.svg, preserving the original Crew 46-unit stone geometry,
#fafafa paper and #09de5e core. Only the platform-required macOS tile is masked;
iOS uses opaque, full-bleed RGB artwork. Generated assets are committed, so
packaging needs neither Pillow nor an SVG renderer. Provider icons are untouched.
"""

from pathlib import Path
import shutil
import xml.etree.ElementTree as ET

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "dist/zeron.svg"
SVG = ET.parse(SOURCE).getroot()
X, Y, WIDTH, HEIGHT = map(float, SVG.attrib["viewBox"].split())
RECT = "{http://www.w3.org/2000/svg}rect"
# This renderer deliberately supports only this mark's six rounded rectangles.
assert WIDTH == HEIGHT and len(SVG) == 6 and all(node.tag == RECT for node in SVG)
assert [node.attrib["fill"] for node in SVG] == ["#000000"] + ["#fafafa"] * 4 + ["#09de5e"]


def raster(size):
    scale = size * 4 / WIDTH
    image = Image.new("RGB", (size * 4, size * 4), "#000000")
    draw = ImageDraw.Draw(image)
    for node in SVG:
        x = (float(node.attrib.get("x", 0)) - X) * scale
        y = (float(node.attrib.get("y", 0)) - Y) * scale
        w = float(node.attrib["width"]) * scale
        h = float(node.attrib["height"]) * scale
        draw.rounded_rectangle((x, y, x + w - 1, y + h - 1),
                               radius=float(node.attrib.get("rx", 0)) * scale,
                               fill=node.attrib["fill"])
    return image.resize((size, size), Image.Resampling.LANCZOS)


def save(image, relative):
    image.save(ROOT / relative)
    alpha = image.getchannel("A").getextrema() if image.mode == "RGBA" else "none"
    print(f"{relative}: {image.width}x{image.height} {image.mode}, alpha={alpha}")


for relative in ("crates/ui/assets/icons/zeron-logo.svg",
                 "apps/landing/public/assets/icons/zeron-logo.svg",
                 "apps/landing/public/favicon.svg"):
    shutil.copyfile(SOURCE, ROOT / relative)

full = raster(1024)
save(full, "dist/zeron.png")
save(full, "apps/ios/Zeron/Assets.xcassets/AppIcon.appiconset/AppIcon1024.png")
for relative in ("apps/landing/public/assets/zeron-app-icon.png",
                 "apps/landing/public/assets/zeron-favicon-v3.png"):
    save(raster(128), relative)

# Standard macOS 100px outer safe area, black 824px tile, no frame or shadow.
# Mask at 4x before downsampling so the tile corners retain smooth alpha.
mask = Image.new("L", (4096, 4096), 0)
ImageDraw.Draw(mask).rounded_rectangle((400, 400, 3695, 3695), radius=741.6, fill=255)
mask = mask.resize((1024, 1024), Image.Resampling.LANCZOS)
mac = Image.new("RGB", (1024, 1024), "#000000")
mac.paste(raster(824), (100, 100))
mac.putalpha(mask)
save(mac, "dist/macos/icon-1024.png")
mac.save(ROOT / "dist/windows/zeron.ico", sizes=[(n, n) for n in (16, 24, 32, 48, 64, 128, 256)])
print("dist/windows/zeron.ico: 16/24/32/48/64/128/256px RGBA frames")
