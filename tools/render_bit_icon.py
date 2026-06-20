#!/usr/bin/env python3
"""
Render the Bit's icon from its real geometry: a flat-shaded stellated icosahedron
(the "neutral" Bit form from src/bit/shapes.ts — IcosahedronGeometry(0.95)
stellated with spikeHeight 0.5). Pure math + Pillow: builds the icosahedron,
stellates each face into a spike, rotates, projects, painter-sorts, flat-shades.

Outputs into ./_iconbuild/. macOS squircle background for app icons; an unmasked
square variant for the Windows Square* logos.

Can't be eyeballed in this session, so it's correct-by-construction and prints a
self-check (cyan coverage, shaded-region count) to catch blank/garbage output.
"""
import math
import os
import subprocess
import sys

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

OUT = os.path.join(os.path.dirname(__file__), "_iconbuild")
os.makedirs(OUT, exist_ok=True)

PHI = (1 + math.sqrt(5)) / 2

# ---- geometry (mirrors src/bit/shapes.ts neutral Bit) ----


def ico_vertices(radius: float = 0.95) -> np.ndarray:
    # 12 vertices: cyclic perms of (0,±1,±φ),(±1,±φ,0),(±φ,0,±1)
    raw = []
    for a in (1, -1):
        for b in (1, -1):
            raw.append((0, a, b * PHI))
            raw.append((a, b * PHI, 0))
            raw.append((b * PHI, 0, a))
    pts = []
    seen = set()
    for p in raw:
        k = (round(p[0], 6), round(p[1], 6), round(p[2], 6))
        if k not in seen:
            seen.add(k)
            pts.append(p)
    pts = np.array(pts, dtype=float)
    pts = pts / np.linalg.norm(pts, axis=1, keepdims=True) * radius
    return pts


def build_faces(verts: np.ndarray):
    """Triangles via edge-length connectivity (order-independent, always correct)."""
    n = len(verts)
    d = np.linalg.norm(verts[:, None, :] - verts[None, :, :], axis=2)
    np.fill_diagonal(d, np.inf)
    L = d.min()
    adj = d < (L * 1.05)  # 30 true edges of an icosahedron
    faces = set()
    for i in range(n):
        nbrs = [k for k in range(n) if adj[i, k]]
        for j in nbrs:
            if j <= i:
                continue
            for k in nbrs:
                if k <= j:
                    continue
                if adj[j, k]:
                    faces.add((i, j, k))
    return list(faces)


def stellate(verts: np.ndarray, faces, spike: float = 0.5):
    """Each face → a triangular-pyramid spike (3 side triangles), apex out along normal."""
    tris = []
    for (i, j, k) in faces:
        a, b, c = verts[i], verts[j], verts[k]
        centroid = (a + b + c) / 3.0
        n = np.cross(b - a, c - a)
        n = n / np.linalg.norm(n)
        if np.dot(n, centroid) < 0:
            n = -n  # ensure outward
        apex = centroid + n * spike
        for p, q in [(a, b), (b, c), (c, a)]:
            tris.append((p, q, apex))
    return tris


# ---- rendering ----

# Tunable look (exposed so the result is easy to re-tune without re-deriving math).
ROT_X = -0.30  # tilt forward
ROT_Y = 0.55   # turn to show depth
LIGHT = np.array([0.5, 0.7, 0.55])  # upper-front-right
LIGHT = LIGHT / np.linalg.norm(LIGHT)
AMBIENT = 0.28
DIFFUSE = 0.85
CYAN_DARK = np.array([0.0, 70.0, 95.0])
CYAN_LIT = np.array([110.0, 245.0, 255.0])


def rot_matrix(ax, ay):
    rx = np.array([[1, 0, 0], [0, math.cos(ax), -math.sin(ax)], [0, math.sin(ax), math.cos(ax)]])
    ry = np.array([[math.cos(ay), 0, math.sin(ay)], [0, 1, 0], [-math.sin(ay), 0, math.cos(ay)]])
    return ry @ rx


def tri_outward_normal(a, b, c):
    n = np.cross(b - a, c - a)
    ln = np.linalg.norm(n)
    if ln < 1e-9:
        return np.array([0.0, 0.0, 1.0])
    n = n / ln
    if np.dot(n, (a + b + c) / 3.0) < 0:
        n = -n
    return n


def render_gem(size: int, mode: str = "squircle", ss: int = 4) -> Image.Image:
    """mode: 'squircle' (macOS app icon), 'square' (Win logos), 'silhouette' (template tray)."""
    S = size * ss
    verts = ico_vertices(0.95)
    tris = stellate(verts, build_faces(verts), 0.5)
    R = rot_matrix(ROT_X, ROT_Y)
    tris = [(R @ a, R @ b, R @ c) for (a, b, c) in tris]

    allv = np.array([v for t in tris for v in t])
    xs, ys = allv[:, 0], allv[:, 1]
    span = max(xs.max() - xs.min(), ys.max() - ys.min())
    margin = 0.11 if mode != "silhouette" else 0.08
    scale = (S * (1 - 2 * margin)) / span
    cx = cy = S / 2.0

    tris.sort(key=lambda t: (t[0][2] + t[1][2] + t[2][2]) / 3.0)  # back to front

    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    if mode == "silhouette":
        # Template image: solid black union of all projected spikes (alpha=255).
        for (a, b, c) in tris:
            poly = [(cx + a[0] * scale, cy - a[1] * scale),
                    (cx + b[0] * scale, cy - b[1] * scale),
                    (cx + c[0] * scale, cy - c[1] * scale)]
            draw.polygon(poly, fill=(0, 0, 0, 255))
    else:
        for (a, b, c) in tris:
            n = tri_outward_normal(a, b, c)
            b_factor = min(1.0, AMBIENT + DIFFUSE * max(0.0, float(np.dot(n, LIGHT))))
            col = CYAN_DARK + (CYAN_LIT - CYAN_DARK) * b_factor
            rgba = (int(col[0]), int(col[1]), int(col[2]), 255)
            poly = [(cx + a[0] * scale, cy - a[1] * scale),
                    (cx + b[0] * scale, cy - b[1] * scale),
                    (cx + c[0] * scale, cy - c[1] * scale)]
            draw.polygon(poly, fill=rgba)

    img = img.resize((size, size), Image.LANCZOS)

    if mode == "silhouette":
        return img

    # Background: dark squircle (macOS) or square (Win), with soft cyan glow behind gem.
    bg = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    bdraw = ImageDraw.Draw(bg)
    if mode == "squircle":
        fill = (12, 16, 22, 255)  # ~#0c1016, matches app --bg
    else:
        fill = (12, 16, 22, 255)
    radius = size * (0.2237 if mode == "squircle" else 0.0)
    bdraw.rounded_rectangle([0, 0, size - 1, size - 1], radius=radius, fill=fill)
    # subtle radial darkening at edges for depth
    vignette = Image.new("L", (size, size), 0)
    vd = ImageDraw.Draw(vignette)
    vd.ellipse([-size * 0.2, -size * 0.2, size * 1.2, size * 1.2], fill=80)
    vignette = vignette.filter(ImageFilter.GaussianBlur(size * 0.18))
    dark = Image.new("RGBA", (size, size), (0, 0, 0, 120))
    bg = Image.composite(dark, bg, vignette)
    # cyan glow behind the gem
    glow = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    gd = ImageDraw.Draw(glow)
    gd.ellipse([size * 0.18, size * 0.18, size * 0.82, size * 0.82], fill=(0, 200, 240, 70))
    glow = glow.filter(ImageFilter.GaussianBlur(size * 0.10))
    bg = Image.alpha_composite(bg, glow)

    out = Image.alpha_composite(bg, img)
    if mode == "squircle":
        mask = Image.new("L", (size, size), 0)
        ImageDraw.Draw(mask).rounded_rectangle([0, 0, size - 1, size - 1], radius=radius, fill=255)
        out.putalpha(mask)
    return out


def self_check(img: Image.Image):
    """Catch blank/garbage output we can't eyeball."""
    arr = np.array(img.convert("RGBA"))
    rgb = arr[:, :, :3].reshape(-1, 3)
    a = arr[:, :, 3].reshape(-1)
    opaque = a > 128
    cyan = opaque & (rgb[:, 2] > 120) & (rgb[:, 2] > rgb[:, 0] + 40) & (rgb[:, 2] > rgb[:, 1])
    cov = cyan.mean()
    # distinct shaded regions: count unique-ish brightness buckets among cyan pixels
    bright = rgb[opaque][:, 2].astype(int) // 32
    regions = len(set(bright.tolist())) if len(bright) else 0
    print(f"  size={img.size[0]:4d}  opaque={opaque.mean():.1%}  cyan={cov:.1%}  shade_buckets={regions}")
    assert 0.03 < cov < 0.80, f"cyan coverage {cov:.1%} looks wrong (expected 3–80%)"
    assert regions >= 4, f"only {regions} shade buckets — shading may be broken"
    return cov


def build_iconset():
    sizes = {16, 32, 64, 128, 256, 512, 1024}
    masters = {}
    for s in sorted(sizes):
        im = render_gem(s, "squircle", ss=4 if s <= 512 else 2)
        self_check(im)
        masters[s] = im
        im.save(os.path.join(OUT, f"g-{s}.png"))
    return masters


def build_square_logos():
    for s in [30, 44, 71, 89, 107, 142, 150, 284, 310]:
        im = render_gem(s, "square", ss=4)
        im.save(os.path.join(OUT, f"sq-{s}.png"))


def main():
    print("rendering app icon (squircle)…")
    masters = build_iconset()
    print("rendering square logos (Windows)…")
    build_square_logos()
    print("rendering tray template…")
    tray = render_gem(256, "silhouette", ss=4)
    tray.save(os.path.join(OUT, "tray-256.png"))
    # tray self-check: should be mostly transparent with a black blob
    arr = np.array(tray)
    black = (arr[:, :, 3] > 128) & (arr[:, :, :3].max(axis=2) < 30)
    print(f"  tray black coverage={black.mean():.1%}")
    assert 0.05 < black.mean() < 0.60

    # Assemble a macOS .iconset → icon.icns
    iconset = os.path.join(OUT, "bit.iconset")
    os.makedirs(iconset, exist_ok=True)
    spec = {16: "icon_16x16.png", 32: "icon_32x32.png", 64: "icon_16x16@2x.png",
            128: "icon_128x128.png", 256: "icon_128x128@2x.png", 256: "icon_256x256.png",
            512: "icon_256x256@2x.png", 1024: "icon_512x512@2x.png"}
    # iconutil wants exact filenames incl. 512x512 + its @2x:
    filespec = [("icon_16x16.png", 16), ("icon_16x16@2x.png", 32),
                ("icon_32x32.png", 32), ("icon_32x32@2x.png", 64),
                ("icon_128x128.png", 128), ("icon_128x128@2x.png", 256),
                ("icon_256x256.png", 256), ("icon_256x256@2x.png", 512),
                ("icon_512x512.png", 512), ("icon_512x512@2x.png", 1024)]
    for name, s in filespec:
        masters[s].save(os.path.join(iconset, name))
    icns_out = os.path.join(OUT, "icon.icns")
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns_out], check=True)
    print(f"  wrote {icns_out}")

    # .ico (Windows) — multi-size
    masters[256].save(os.path.join(OUT, "icon.ico"),
                      sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    print("done.")


if __name__ == "__main__":
    main()
