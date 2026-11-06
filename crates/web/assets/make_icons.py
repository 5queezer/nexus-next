#!/usr/bin/env python3
"""Regenerates the PWA icons: python3 make_icons.py (needs Pillow). A white N on the accent green;
the maskable one keeps the glyph inside the 80% safe zone and fills the whole square."""
from PIL import Image, ImageDraw

GREEN = (31, 95, 74, 255)
WHITE = (255, 255, 255, 255)
SCALE = 4


def icon(size, glyph, rounded):
    big = size * SCALE
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    if rounded:
        d.rounded_rectangle((0, 0, big - 1, big - 1), radius=big // 5, fill=GREEN)
    else:
        d.rectangle((0, 0, big, big), fill=GREEN)
    box = big * glyph
    left, top = (big - box) / 2, (big - box) / 2
    stem = box * 0.2
    right = left + box
    bottom = top + box
    d.rectangle((left, top, left + stem, bottom), fill=WHITE)
    d.rectangle((right - stem, top, right, bottom), fill=WHITE)
    d.polygon(
        [(left, top), (left + stem * 1.15, top), (right, bottom), (right - stem * 1.15, bottom)],
        fill=WHITE,
    )
    return img.resize((size, size), Image.LANCZOS)


icon(192, 0.5, True).save("icon-192.png", optimize=True)
icon(512, 0.5, True).save("icon-512.png", optimize=True)
icon(512, 0.4, False).save("icon-maskable-512.png", optimize=True)
icon(180, 0.5, False).save("apple-touch-icon.png", optimize=True)
