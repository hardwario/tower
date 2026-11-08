#!/usr/bin/env python3
"""Convert a PNG into a tower-hmi 1-bit `Image` (Rust source).

Reads an 8-bit PNG (grayscale, RGB, with or without alpha, or palette; not interlaced),
composites it on white, optionally trims it to its ink, scales it to the size asked for with an
area-averaging filter, thresholds it to ink and paper, and writes a Rust file defining the image:

    pub const NAME: Image = Image::new(WIDTH, HEIGHT, &[...]);

Rows are packed most significant bit first, a set bit ink: the layout `tower_hmi::image`
documents. With `--preview`, it also writes the result as a PNG scaled up 4 times, to check what
the threshold kept before flashing. Stdlib only.

    python3 -I crates/hmi/tools/image.py logo.png --width 124 --trim --name LOGO --out logo.rs --preview logo-1bit.png

A vector logo can be rendered to PNG first (on macOS: `qlmanage -t -s 1600 -o . logo.svg`).
"""

import argparse
import struct
import zlib


def read_png(path):
    """Return (width, height, rows of (r, g, b, a) tuples)."""
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"{path}: not a PNG")
    pos, idat, palette, trns = 8, b"", None, None
    while pos < len(data):
        n, kind = struct.unpack(">I4s", data[pos : pos + 8])
        body = data[pos + 8 : pos + 8 + n]
        pos += 12 + n
        if kind == b"IHDR":
            w, h, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
            if depth != 8 or interlace:
                raise SystemExit(f"{path}: only 8-bit, non-interlaced PNGs (got depth {depth}, interlace {interlace})")
        elif kind == b"PLTE":
            palette = [tuple(body[i : i + 3]) for i in range(0, len(body), 3)]
        elif kind == b"tRNS":
            trns = body
        elif kind == b"IDAT":
            idat += body
    channels = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[ctype]
    raw = zlib.decompress(idat)
    stride = w * channels
    rows, prev = [], bytearray(stride)
    for y in range(h):
        f = raw[y * (stride + 1)]
        line = bytearray(raw[y * (stride + 1) + 1 : (y + 1) * (stride + 1)])
        for i in range(stride):
            a = line[i - channels] if i >= channels else 0
            b = prev[i]
            c = prev[i - channels] if i >= channels else 0
            if f == 1:
                line[i] = (line[i] + a) & 0xFF
            elif f == 2:
                line[i] = (line[i] + b) & 0xFF
            elif f == 3:
                line[i] = (line[i] + (a + b) // 2) & 0xFF
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[i] = (line[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 0xFF
        prev = line
        px = []
        for x in range(w):
            v = line[x * channels : (x + 1) * channels]
            if ctype == 0:
                px.append((v[0], v[0], v[0], 255))
            elif ctype == 2:
                px.append((v[0], v[1], v[2], 255))
            elif ctype == 3:
                alpha = trns[v[0]] if trns and v[0] < len(trns) else 255
                px.append((*palette[v[0]], alpha))
            elif ctype == 4:
                px.append((v[0], v[0], v[0], v[1]))
            else:
                px.append(tuple(v))
        rows.append(px)
    return w, h, rows


def darkness(rows):
    """Per pixel 0.0 (white paper) .. 1.0 (black ink), composited on white."""
    out = []
    for row in rows:
        line = []
        for r, g, b, a in row:
            lum = (0.299 * r + 0.587 * g + 0.114 * b) / 255
            line.append((1 - lum) * a / 255)
        out.append(line)
    return out


def trim(dark, level=0.5):
    ys = [y for y, row in enumerate(dark) if max(row) >= level]
    xs = [x for x in range(len(dark[0])) if any(row[x] >= level for row in dark)]
    if not ys:
        raise SystemExit("no ink to trim to")
    return [row[xs[0] : xs[-1] + 1] for row in dark[ys[0] : ys[-1] + 1]]


def scale(dark, w, h):
    """Area-average `dark` down (or up) to w x h."""
    sh, sw = len(dark), len(dark[0])
    out = []
    for y in range(h):
        y0, y1 = y * sh / h, (y + 1) * sh / h
        line = []
        for x in range(w):
            x0, x1 = x * sw / w, (x + 1) * sw / w
            total = area = 0.0
            for sy in range(int(y0), min(sh, int(y1 + 0.999))):
                wy = min(y1, sy + 1) - max(y0, sy)
                for sx in range(int(x0), min(sw, int(x1 + 0.999))):
                    wx = min(x1, sx + 1) - max(x0, sx)
                    total += dark[sy][sx] * wx * wy
                    area += wx * wy
            line.append(total / area if area else 0.0)
        out.append(line)
    return out


def pack(bits):
    stride = (len(bits[0]) + 7) // 8
    out = bytearray()
    for row in bits:
        line = bytearray(stride)
        for x, ink in enumerate(row):
            if ink:
                line[x // 8] |= 0x80 >> (x % 8)
        out += line
    return out


def preview(bits, path, s=4):
    h, w = len(bits), len(bits[0])
    raw = bytearray()
    for y in range(h * s):
        raw.append(0)
        for x in range(w * s):
            raw.append(0x30 if bits[y // s][x // s] else 0xD8)

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w * s, h * s, 8, 0, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw))) + chunk(b"IEND", b"")
    open(path, "wb").write(png)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("png")
    ap.add_argument("--width", type=int, help="target width in pixels (height follows the aspect)")
    ap.add_argument("--height", type=int, help="target height in pixels (width follows the aspect)")
    ap.add_argument("--trim", action="store_true", help="crop to the ink first")
    ap.add_argument("--threshold", type=float, default=0.5, help="ink at this darkness and up (0..1)")
    ap.add_argument("--invert", action="store_true", help="swap ink and paper")
    ap.add_argument("--name", default="IMAGE", help="the Rust constant's name")
    ap.add_argument("--source", default="", help="where the image came from, for the file's header ('; ' starts a line)")
    ap.add_argument("--out", required=True, help="the Rust file to write")
    ap.add_argument("--preview", help="also write the 1-bit result as a PNG, scaled up 4 times")
    a = ap.parse_args()

    _, _, rows = read_png(a.png)
    dark = darkness(rows)
    if a.trim:
        dark = trim(dark)
    sh, sw = len(dark), len(dark[0])
    if a.width and a.height:
        w, h = a.width, a.height
    elif a.width:
        w, h = a.width, max(1, round(sh * a.width / sw))
    elif a.height:
        w, h = max(1, round(sw * a.height / sh)), a.height
    else:
        w, h = sw, sh
    small = scale(dark, w, h)
    bits = [[(v >= a.threshold) != a.invert for v in row] for row in small]
    data = pack(bits)
    stride = (w + 7) // 8
    lines = ["// @generated by crates/hmi/tools/image.py; do not edit, re-run the tool. From:"]
    lines += ["//   " + part.strip() for part in (a.source or a.png).split(";")]
    lines += [f"// {w} x {h} pixels, threshold {a.threshold}.", ""]
    lines += [
        "use tower::hmi::Image;",
        "",
        f"pub const {a.name}: Image = Image::new(",
        f"    {w},",
        f"    {h},",
        "    &[",
    ]
    for y in range(h):
        lines.append("        " + " ".join(f"0x{b:02X}," for b in data[y * stride : (y + 1) * stride]))
    lines += ["    ],", ");", ""]
    open(a.out, "w").write("\n".join(lines))
    if a.preview:
        preview(bits, a.preview)
    print(f"wrote {a.out}: {w} x {h} pixels, {len(data)} bytes")


if __name__ == "__main__":
    main()
