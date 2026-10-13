"""Draws the app icon (three faders on a dark tile) and writes assets/app.ico. Standard library only."""
import math, struct, zlib, os

BG, EDGE, TRACK, X, GLOW = (12, 16, 25), (42, 53, 80), (58, 72, 108), (74, 120, 255), (0, 63, 250)

def seg_dist(px, py, ax, ay, bx, by):
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))

def render(n):
    ss = 4
    px = []
    # the header mark in ui/index.html, on a 40x40 grid: three tracks, and a cap on each
    tracks = [(11, 7, 11, 33), (20, 7, 20, 33), (29, 7, 29, 33)]
    caps = [(7.5, 25, 14.5, 25), (16.5, 13, 23.5, 13), (25.5, 20, 32.5, 20)]
    inset, radius = 0.5, 9.0
    for y in range(n):
        for x in range(n):
            acc = [0.0, 0.0, 0.0, 0.0]
            for sy in range(ss):
                for sx in range(ss):
                    u = (x + (sx + .5) / ss) / n * 40
                    v = (y + (sy + .5) / ss) / n * 40
                    # rounded tile
                    qx = max(abs(u - 20) - (20 - inset - radius), 0)
                    qy = max(abs(v - 20) - (20 - inset - radius), 0)
                    dtile = math.hypot(qx, qy) - radius
                    if dtile > 0:
                        continue
                    col = list(EDGE if dtile > -0.9 * 40 / n * 1.2 else BG)
                    d = min(seg_dist(u, v, *s) for s in caps)
                    w = 2.5 if n > 24 else 3.0
                    glow = max(0.0, 1 - d / 9) ** 2 * .55
                    col = [c + (g - c) * glow for c, g in zip(col, GLOW)]
                    # the tracks under the caps, then the caps
                    for segs, sw, ink in ((tracks, 1.25 if n > 24 else 1.6, TRACK), (caps, w, X)):
                        ds = min(seg_dist(u, v, *s) for s in segs)
                        if ds < sw:
                            col = list(ink)
                        elif ds < sw + .8:
                            k = (ds - sw) / .8
                            col = [a + (b - a) * k for a, b in zip(ink, col)]
                    acc[0] += col[0]; acc[1] += col[1]; acc[2] += col[2]; acc[3] += 255
            k = ss * ss
            a = acc[3] / k
            px.append(tuple(int(round(c / max(acc[3] / 255, 1e-9))) for c in acc[:3]) + (int(round(a)),))
    return px

def png(n, px):
    raw = b"".join(b"\0" + bytes(c for p in px[r * n:(r + 1) * n] for c in p) for r in range(n))
    chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", n, n, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")

sizes = [16, 24, 32, 48, 64, 256]
images = [png(n, render(n)) for n in sizes]
out = struct.pack("<HHH", 0, 1, len(sizes))
offset = 6 + 16 * len(sizes)
for n, im in zip(sizes, images):
    out += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(im), offset)
    offset += len(im)
here = os.path.dirname(os.path.abspath(__file__))
open(os.path.join(here, "app.ico"), "wb").write(out + b"".join(images))
open(os.path.join(here, "icon-256.png"), "wb").write(images[-1])
print("wrote app.ico", len(out) + sum(map(len, images)), "bytes")
