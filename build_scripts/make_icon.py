#!/usr/bin/env python3
"""Draws assets/icon.png (1024x1024) and assets/icon-256.png. Needs Pillow."""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFilter

S = 4  # supersampling
N = 1024 * S
out = Path(__file__).resolve().parent.parent / "assets"


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def px(*v):
    return [x * S for x in v]


img = Image.new("RGBA", (N, N), (0, 0, 0, 0))

# Tile with a soft shadow (macOS icon grid: 824 px tile in a 1024 px canvas).
x0, y0, x1, y1, r = px(100, 100, 924, 924, 185)
shadow = Image.new("RGBA", (N, N), (0, 0, 0, 0))
ImageDraw.Draw(shadow).rounded_rectangle([x0, y0 + 14 * S, x1, y1 + 14 * S], r, fill=(0, 0, 0, 90))
img.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(18 * S)))

grad = Image.new("RGBA", (N, N))
gd = ImageDraw.Draw(grad)
top, bottom = (92, 156, 255), (32, 82, 214)
for y in range(N):
    gd.line([(0, y), (N, y)], fill=lerp(top, bottom, max(0, min(1, (y - y0) / (y1 - y0)))) + (255,))
mask = Image.new("L", (N, N), 0)
ImageDraw.Draw(mask).rounded_rectangle([x0, y0, x1, y1], r, fill=255)
img.paste(grad, (0, 0), mask)

# Page with a folded top-right corner.
d = ImageDraw.Draw(img)
L, T, R, B, fold, pr = 262, 214, 762, 810, 120, 34
page = Image.new("L", (N, N), 0)
pd = ImageDraw.Draw(page)
pd.rounded_rectangle(px(L, T, R, B), pr * S, fill=255)
pd.polygon(px(R - fold, T - 1, R + 1, T - 1, R + 1, T + fold), fill=0)
page_shadow = Image.new("RGBA", (N, N), (10, 30, 90, 0))
page_shadow.putalpha(page.point(lambda a: a * 110 // 255).transform((N, N), Image.AFFINE, (1, 0, 0, 0, 1, -10 * S)))
img.alpha_composite(page_shadow.filter(ImageFilter.GaussianBlur(12 * S)))
img.paste((255, 255, 255, 255), (0, 0), page)
d.polygon(px(R - fold, T, R - fold, T + fold, R, T + fold), fill=(205, 218, 240, 255))

# Text lines and a caret.
line = (176, 192, 218, 255)
lw = 22
d.rounded_rectangle(px(L + 64, T + 92, R - fold - 40, T + 92 + lw + 6), 14 * S, fill=(32, 82, 214, 255))
ys = [T + 196, T + 274, T + 352, T + 430, T + 508]
ends = [R - 64, R - 64, R - 120, R - 64, L + 250]
for y, e in zip(ys, ends):
    d.rounded_rectangle(px(L + 64, y, e, y + lw), (lw // 2) * S, fill=line)
cx = ends[-1] + 26
d.rounded_rectangle(px(cx, ys[-1] - 26, cx + 14, ys[-1] + lw + 26), 7 * S, fill=(32, 82, 214, 255))

big = img.resize((1024, 1024), Image.LANCZOS)
big.save(out / "icon.png", optimize=True)
big.resize((256, 256), Image.LANCZOS).save(out / "icon-256.png", optimize=True)
print("wrote", out / "icon.png", out / "icon-256.png")
