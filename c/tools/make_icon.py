"""Writes the app icon (.ico with PNG images) for the C build.

Same geometry as src/icon.rs in the Rust build: the "afterimage" mark in white
on an indigo rounded tile. PNG-compressed because UPX can't compress icons.

Usage: python make_icon.py out.ico
"""

import struct
import sys
import zlib

CIRCLES = [(21.0, 9.0, 0.16), (29.0, 10.2, 0.38), (38.5, 11.5, 1.0)]
CIRCLE_Y = 32.0
TILE_RADIUS = 15.0
TOP_LEFT = (0x6A / 255, 0x5A / 255, 0xE0 / 255)
BOTTOM_RIGHT = (0x1E / 255, 0x1A / 255, 0x4A / 255)
SUPERSAMPLE = 4
SIZES = [16, 32, 48]


def over(dst, rgb, alpha):
    k = 1 - alpha
    return (rgb[0] * alpha + dst[0] * k, rgb[1] * alpha + dst[1] * k, rgb[2] * alpha + dst[2] * k, alpha + dst[3] * k)


def in_rounded_rect(u, v, inset):
    half, r = 32 - inset, TILE_RADIUS - inset
    dx = max(abs(u - 32) - (half - r), 0)
    dy = max(abs(v - 32) - (half - r), 0)
    return dx * dx + dy * dy <= r * r


def tile(u, v):
    if not in_rounded_rect(u, v, 0):
        return (0, 0, 0, 0)
    t = min(max((u + v) / 128, 0), 1)
    rgb = tuple(TOP_LEFT[i] + (BOTTOM_RIGHT[i] - TOP_LEFT[i]) * t for i in range(3))
    c = over((0, 0, 0, 0), rgb, 1)
    if not in_rounded_rect(u, v, 1):
        c = over(c, (1, 1, 1), 0.14)
    for cx, r, opacity in CIRCLES:
        if (u - cx) ** 2 + (v - CIRCLE_Y) ** 2 <= r * r:
            c = over(c, (1, 1, 1), opacity)
    return c


def render(size):
    rows = []
    n = SUPERSAMPLE * SUPERSAMPLE
    for y in range(size):
        row = bytearray()
        for x in range(size):
            acc = [0.0] * 4
            for sy in range(SUPERSAMPLE):
                for sx in range(SUPERSAMPLE):
                    fx = x + (sx + 0.5) / SUPERSAMPLE
                    fy = y + (sy + 0.5) / SUPERSAMPLE
                    c = tile(fx / size * 64, fy / size * 64)
                    for i in range(4):
                        acc[i] += c[i]
            a = acc[3] / n
            px = [round(acc[i] / n / a * 255) if a > 0 else 0 for i in range(3)]
            row += bytes([min(max(v, 0), 255) for v in px] + [round(a * 255)])
        rows.append(bytes(row))
    return rows


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    return a if pa <= pb and pa <= pc else b if pb <= pc else c


def filter_rows(rows, bpp=4, fixed=None):
    """PNG row filters: `fixed` uses one filter for every row, otherwise each row
    gets the filter leaving the smallest residuals (the usual heuristic)."""
    out = bytearray()
    prev = bytes(len(rows[0]))
    for row in rows:
        candidates = []
        for kind in range(5) if fixed is None else [fixed]:
            f = bytearray()
            for i, x in enumerate(row):
                a = row[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                pred = (0, a, b, (a + b) // 2, paeth(a, b, c))[kind]
                f.append((x - pred) & 0xFF)
            cost = sum(v if v < 128 else 256 - v for v in f)
            candidates.append((cost, kind, f))
        _, kind, f = min(candidates)
        out += bytes([kind]) + f
        prev = row
    return bytes(out)


def png(size):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    # UPX can't compress icons, so try every filter choice and deflate
    # strategy and keep the smallest.
    rows = render(size)
    best = None
    for fixed in [None, 0, 1, 2, 3, 4]:
        raw = filter_rows(rows, fixed=fixed)
        for strategy in (zlib.Z_DEFAULT_STRATEGY, zlib.Z_FILTERED):
            c = zlib.compressobj(9, zlib.DEFLATED, 15, 9, strategy)
            data = c.compress(raw) + c.flush()
            if best is None or len(data) < len(best):
                best = data
    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", best) + chunk(b"IEND", b"")


def main(out):
    images = [png(s) for s in SIZES]
    ico = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for size, data in zip(SIZES, images):
        ico += struct.pack("<BBBBHHII", size, size, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    with open(out, "wb") as f:
        f.write(ico + b"".join(images))


if __name__ == "__main__":
    main(sys.argv[1])
