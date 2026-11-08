#!/usr/bin/env python3
"""Generate tower-hmi's Spleen fonts from the upstream release tarball.

Spleen (https://github.com/fcambus/spleen, BSD-2-Clause, (c) Frederic Cambus) is a monospaced
bitmap font in six sizes. This script reads the BDF files straight out of the pinned release
tarball (checksum-verified, nothing extracted to disk), and writes:

  * `src/fonts/spleen/<W>x<H>-<block>.bin`: the glyph bitmaps of one block, glyph after glyph,
    each `H` rows of `ceil(W / 8)` bytes, most significant bit = leftmost pixel (the BDF row
    format as is, and the layout `tower_hmi::font` blits);
  * `src/fonts/spleen.rs`: the Rust index (one module per size, a `Block` per blob, and the
    preset `Font`s).

Fonts are split into blocks (ASCII, Latin-1, ...) so an application links only the glyphs it
uses: the linker drops every block no font of the application refers to. Which blocks each size
gets is the `SIZES` table below; edit it and re-run to change the set.

Usage (stdlib only, run from the repository root):

    gh release download 2.2.0 -R fcambus/spleen -p 'spleen-2.2.0.tar.gz' -D /tmp/spleen
    python3 -I crates/hmi/tools/spleen.py /tmp/spleen/spleen-2.2.0.tar.gz
"""

import hashlib
import re
import sys
import tarfile
from pathlib import Path

VERSION = "2.2.0"
SHA256 = "ec42925c6b56d2138c862b2f97147c872e472f674bf03423417d827a08d69a89"

CRATE = Path(__file__).resolve().parent.parent
OUT_BIN = CRATE / "src" / "fonts" / "spleen"
OUT_RS = CRATE / "src" / "fonts" / "spleen.rs"

# Code point sets of the blocks. A block is "dense" (a run `first..first + count`, every code
# point present) or "sparse" (a sorted code point table, binary-searched).
ASCII = list(range(0x20, 0x7F))
LATIN1 = list(range(0xA0, 0x100))
LATIN_EXT_A = list(range(0x100, 0x180))
# UI symbols: Greek, punctuation, currency, arrows, mathematical operators, technical, block
# elements (bar graphs, gauges), geometric shapes, miscellaneous symbols, angle brackets and the
# supplemental arrows. Each size gets whichever of these it actually has.
SYMBOL_RANGES = [
    (0x0370, 0x03FF),
    (0x2010, 0x206F),
    (0x20A0, 0x20CF),
    (0x2190, 0x21FF),
    (0x2200, 0x22FF),
    (0x2300, 0x23FF),
    (0x2580, 0x259F),
    (0x25A0, 0x25FF),
    (0x2600, 0x26FF),
    (0x27E8, 0x27E9),
    (0x2B00, 0x2BFF),
]
# The characters of a numeric readout (temperature, clock, percentage): the big sizes get this
# as a small block of its own, so a large display font costs a few KB instead of a whole ASCII.
NUMERIC = sorted(ord(c) for c in " %+,-./0123456789:°")

# Blocks per size: (Rust name, code points or None for the symbol ranges). The preset `Font` of
# each size (`SPLEEN_<W>X<H>`) is `PRESET` (ASCII + Latin-1); 32x64 keeps to ASCII, Latin-1 and
# the numeric subset, as its glyphs are 256 bytes each.
BLOCKS_FULL = [("ASCII", ASCII), ("LATIN1", LATIN1), ("LATIN_EXT_A", LATIN_EXT_A), ("SYMBOLS", None)]
SIZES = {
    "5x8": BLOCKS_FULL,
    "6x12": BLOCKS_FULL,
    "8x16": BLOCKS_FULL,
    "12x24": BLOCKS_FULL,
    "16x32": BLOCKS_FULL + [("NUMERIC", NUMERIC)],
    "32x64": [("ASCII", ASCII), ("LATIN1", LATIN1), ("NUMERIC", NUMERIC)],
}
PRESET = ["ASCII", "LATIN1"]

DOCS = {
    "ASCII": "Printable ASCII, U+0020 to U+007E.",
    "LATIN1": "Latin-1 Supplement, U+00A0 to U+00FF (degree, micro, Western European letters).",
    "LATIN_EXT_A": "Latin Extended-A, U+0100 to U+017F (Czech, Slovak, Polish, Hungarian, ...).",
    "SYMBOLS": "UI symbols this size has: arrows, block elements, shapes, Greek, punctuation.",
    "NUMERIC": "A numeric readout: space, `%+,-./`, the digits, `:` and the degree sign.",
}


def parse_bdf(text):
    """Return (width, height, ascent, {codepoint: bytes}) of a fixed-cell BDF font."""
    bbox = re.search(r"^FONTBOUNDINGBOX (\d+) (\d+) (-?\d+) (-?\d+)$", text, re.M)
    w, h, _, yoff = (int(v) for v in bbox.groups())
    ascent = h + yoff  # rows above the baseline
    stride = (w + 7) // 8
    glyphs = {}
    for m in re.finditer(r"^STARTCHAR.*?^ENDCHAR$", text, re.M | re.S):
        g = m.group(0)
        enc = int(re.search(r"^ENCODING (-?\d+)$", g, re.M).group(1))
        if enc < 0:
            continue
        gb = re.search(r"^BBX (\d+) (\d+) (-?\d+) (-?\d+)$", g, re.M).groups()
        if tuple(int(v) for v in gb) != (w, h, 0, yoff):
            raise SystemExit(f"U+{enc:04X}: glyph box {gb} differs from the font cell; not supported")
        rows = g.split("BITMAP\n", 1)[1].split("\nENDCHAR", 1)[0].split()
        if len(rows) != h:
            raise SystemExit(f"U+{enc:04X}: {len(rows)} bitmap rows, expected {h}")
        data = b"".join(bytes.fromhex(r.rjust(stride * 2, "0")[: stride * 2]) for r in rows)
        glyphs[enc] = data
    return w, h, ascent, glyphs


# Code points that are blank by design. Any other glyph with an empty bitmap is a placeholder
# (Spleen 5x8 encodes the Latin-1 letters but leaves them empty) and is left out, so the text
# renderer falls back to `?` instead of drawing nothing.
SPACES = {0x20, 0xA0, *range(0x2000, 0x200B), 0x202F, 0x205F}


def block_codes(name, codes, glyphs):
    if codes is None:
        codes = [c for lo, hi in SYMBOL_RANGES for c in range(lo, hi + 1)]
    codes = [c for c in codes if c in glyphs and (c in SPACES or any(glyphs[c]))]
    # A block of nothing but spaces is no block.
    return codes if any(c not in SPACES for c in codes) else []


def is_dense(codes):
    return all(b == a + 1 for a, b in zip(codes, codes[1:]))


def main(argv):
    if len(argv) != 2:
        raise SystemExit(__doc__)
    tarball = Path(argv[1])
    digest = hashlib.sha256(tarball.read_bytes()).hexdigest()
    if digest != SHA256:
        raise SystemExit(f"{tarball}: sha256 {digest} is not the pinned spleen {VERSION} release")

    OUT_BIN.mkdir(parents=True, exist_ok=True)
    for old in OUT_BIN.glob("*.bin"):
        old.unlink()

    rs = [
        f"// @generated by crates/hmi/tools/spleen.py from spleen-{VERSION}.tar.gz",
        f"// (sha256 {SHA256}).",
        "// Do not edit; re-run the generator.",
        "//",
        "// Spleen is (c) 2018-2026 Frederic Cambus, BSD-2-Clause (see crates/hmi/LICENSE-spleen).",
        "",
        "use crate::font::{Block, Font};",
        "",
    ]
    total = 0
    with tarfile.open(tarball) as tar:
        members = {Path(m.name).name: m for m in tar.getmembers()}
        for size, blocks in SIZES.items():
            text = tar.extractfile(members[f"spleen-{size}.bdf"]).read().decode("ascii")
            w, h, ascent, glyphs = parse_bdf(text)
            mod = f"spleen_{size}"
            rs += [
                f"/// Spleen {size}: {w} x {h} pixel cells, baseline {ascent} rows from the top.",
                f"pub mod {mod} {{",
                "    use super::{Block, Font};",
                "",
                f"    /// A {size} font of the given blocks (in lookup order).",
                "    pub const fn with(blocks: &'static [&'static Block]) -> Font {",
                f"        Font::new({w}, {h}, {ascent}, blocks)",
                "    }",
            ]
            present = []
            for name, codes in blocks:
                codes = block_codes(name, codes, glyphs)
                if not codes:
                    continue
                present.append(name)
                fname = f"{size}-{name.lower().replace('_', '-')}.bin"
                blob = b"".join(glyphs[c] for c in codes)
                (OUT_BIN / fname).write_bytes(blob)
                total += len(blob)
                rs += ["", f"    /// {DOCS[name]}", f"    ///", f"    /// {len(codes)} glyphs, {len(blob)} bytes."]
                data = f'include_bytes!("spleen/{fname}")'
                if is_dense(codes):
                    rs.append(f"    pub static {name}: Block = Block::dense(0x{codes[0]:04X}, {len(codes)}, {data});")
                else:
                    rs.append(f"    pub static {name}: Block = Block::sparse(&{name}_CODES, {data});")
                    rs.append(f"    static {name}_CODES: [u16; {len(codes)}] = [")
                    for i in range(0, len(codes), 10):
                        rs.append("        " + " ".join(f"0x{c:04X}," for c in codes[i : i + 10]))
                    rs.append("    ];")
            rs += ["}", ""]
            names = [b for b in PRESET if b in present]
            preset = ", ".join(f"&{mod}::{b}" for b in names)
            listed = ", ".join(f"`{mod}::{b}`" for b in names)
            rs += [
                f"/// Spleen {size}: {listed}.",
                f"pub static SPLEEN_{size.upper()}: Font = {mod}::with(&[{preset}]);",
                "",
            ]
            if "NUMERIC" in present:
                rs += [
                    f"/// Spleen {size}, numeric readout subset only (`{mod}::NUMERIC`).",
                    f"pub static SPLEEN_{size.upper()}_NUMERIC: Font = {mod}::with(&[&{mod}::NUMERIC]);",
                    "",
                ]
    OUT_RS.write_text("\n".join(rs).rstrip() + "\n")
    print(f"wrote {OUT_RS.relative_to(CRATE)} and {total} bytes of glyphs in {OUT_BIN.relative_to(CRATE)}")


if __name__ == "__main__":
    main(sys.argv)
