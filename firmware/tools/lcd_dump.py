#!/usr/bin/env python3
"""Render a TOWER LCD Module's frame buffer to a PNG.

Runs `tower exec "lcd dump"` against an app that mounts `lcd::SHELL` (or reads a saved dump from
a file), and writes the 128 x 128 frame scaled up 4 times, ink dark on a pale grey: what the
firmware drew, as the panel should show it. Stdlib only.

    python3 tools/lcd_dump.py screen.png                 # the attached device
    python3 tools/lcd_dump.py screen.png -d /dev/cu.usbserial-XXXX
    python3 tools/lcd_dump.py screen.png --input dump.txt
"""

import argparse
import struct
import subprocess
import sys
import zlib

SCALE = 4
INK, PAPER = 0x30, 0xD8


def rows_from(text):
    rows = [line.strip() for line in text.splitlines() if len(line.strip()) == 32]
    if len(rows) != 128:
        raise SystemExit(f"expected 128 rows of 32 hex digits, got {len(rows)}:\n{text[:400]}")
    return [bytes.fromhex(r) for r in rows]


def png(rows, path):
    w = h = 128 * SCALE
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        row = rows[y // SCALE]
        for x in range(w):
            px = x // SCALE
            raw.append(INK if row[px // 8] & (0x80 >> (px % 8)) else PAPER)

    def chunk(kind, data):
        c = struct.pack(">I", len(data)) + kind + data
        return c + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 0, 0, 0, 0)
    out = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(raw))) + chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("png", help="output file")
    ap.add_argument("-d", "--device", help="serial device (passed to tower -d)")
    ap.add_argument("--input", help="read a saved `lcd dump` output instead of the device")
    a = ap.parse_args()
    if a.input:
        text = open(a.input).read()
    else:
        cmd = ["tower"] + (["-d", a.device] if a.device else []) + ["exec", "lcd dump"]
        # The dump is refused while a flush holds the frame (a few ms at a time, but most of the
        # time in an animated game): retry.
        for _ in range(30):
            r = subprocess.run(cmd, capture_output=True, text=True)
            if r.returncode == 0:
                break
        else:
            sys.exit(f"{' '.join(cmd)} failed ({r.returncode}): {r.stderr.strip() or r.stdout.strip()}")
        text = r.stdout
    png(rows_from(text), a.png)
    print(f"wrote {a.png}")


if __name__ == "__main__":
    main()
