#!/usr/bin/env python3
"""Sanity-check a PNG written by `ovvc-gui --screenshot`.

The point is not a golden image; it is to prove the real eframe/wgpu window
painted a full frame with the terminal palette, text and (for the populated
case) accent-coloured chrome. Two screenshots can also be compared to prove
that different state reaches the framebuffer.

Usage:
  check-gui-screenshot.py offline shot.png
  check-gui-screenshot.py populated shot.png --compare offline.png
"""

import argparse
import sys
from collections import Counter

try:
    import gi

    gi.require_version("GdkPixbuf", "2.0")
    from gi.repository import GdkPixbuf
except Exception as error:  # pragma: no cover - environment check
    raise SystemExit(f"check-gui-screenshot: python3-gi/GdkPixbuf required: {error}")


def load(path):
    pixbuf = GdkPixbuf.Pixbuf.new_from_file(path)
    width = pixbuf.get_width()
    height = pixbuf.get_height()
    stride = pixbuf.get_rowstride()
    channels = pixbuf.get_n_channels()
    raw = pixbuf.get_pixels()
    pixels = bytearray()
    for y in range(height):
        row = raw[y * stride : y * stride + width * channels]
        if channels == 4:
            pixels += row
        else:
            for x in range(width):
                pixels += row[x * channels : x * channels + 3] + b"\xff"
    return width, height, bytes(pixels)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["offline", "populated"])
    parser.add_argument("image")
    parser.add_argument("--compare", help="another screenshot that must differ")
    parser.add_argument("--min-diff", type=int, default=10_000)
    args = parser.parse_args()

    width, height, pixels = load(args.image)
    if (width, height) != (1180, 760):
        raise SystemExit(f"expected a 1180x760 frame, got {width}x{height}")

    colors = Counter(
        (pixels[index], pixels[index + 1], pixels[index + 2], pixels[index + 3])
        for index in range(0, len(pixels), 4)
    )
    total = width * height
    dark = sum(count for color, count in colors.items() if max(color[:3]) < 32)
    bright = sum(count for color, count in colors.items() if min(color[:3]) > 160)
    accent = sum(
        count
        for color, count in colors.items()
        if color[1] > color[0] + 20 and color[1] > color[2] + 20
    )

    print(
        f"{args.image}: {width}x{height} colours={len(colors)} "
        f"dark={dark / total:.1%} bright={bright} accent={accent}"
    )

    if len(colors) < 500:
        raise SystemExit(f"frame is too uniform: only {len(colors)} colours")
    if dark < total * 0.25:
        raise SystemExit(f"terminal background covers only {dark / total:.1%} of the frame")
    if bright < 100:
        raise SystemExit(f"only {bright} bright pixels; text/borders did not paint")
    if args.mode == "populated":
        if len(colors) < 1_000:
            raise SystemExit(f"populated frame has only {len(colors)} colours")
        if bright < 200:
            raise SystemExit(f"populated frame has only {bright} bright pixels")
        if accent < 100:
            raise SystemExit(f"populated frame has only {accent} accent pixels")

    if args.compare:
        other_width, other_height, other = load(args.compare)
        if (other_width, other_height) != (width, height):
            raise SystemExit("screenshots have different dimensions")
        differing = sum(
            1
            for index in range(0, len(pixels), 4)
            if pixels[index : index + 4] != other[index : index + 4]
        )
        print(f"difference from {args.compare}: {differing} pixels")
        if differing < args.min_diff:
            raise SystemExit(
                f"screenshots differ by only {differing} pixels (want {args.min_diff})"
            )


if __name__ == "__main__":
    main()
