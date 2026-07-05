#!/usr/bin/env python3
"""Wire-bump guard: golden vectors must not change without a PROTOCOL_VERSION bump.

postcard is not self-describing, so the golden vectors in tests/golden.rs are the
byte-level definition of the wire format. The `test` CI job guarantees the goldens
match the code; this script closes the other half of the loop: if the golden BYTES
changed relative to the latest release tag, PROTOCOL_VERSION must have changed too.
Together the two checks make "wire change => version bump" mechanical instead of
a comment-enforced convention.

Usage: python3 tools/check_wire_bump.py   (run from the repo root; needs tags fetched)
Exit codes: 0 ok, 1 violation, 2 cannot determine baseline.
"""

import re
import subprocess
import sys

GOLDEN = "tests/golden.rs"
LIB = "src/lib.rs"

HEX = re.compile(r"0x[0-9a-fA-F_]+")
VER = re.compile(r"pub const PROTOCOL_VERSION:\s*u8\s*=\s*(\d+)")


def run(*args: str) -> str:
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def at_tag(tag: str, path: str) -> str:
    return run("git", "show", f"{tag}:{path}")


def golden_bytes(source: str) -> list[str]:
    # Every hex literal in file order. This covers the expected wire bytes AND the
    # test inputs that produce them — a change to either is a golden change.
    return HEX.findall(source)


def protocol_version(source: str) -> int:
    m = VER.search(source)
    if not m:
        sys.exit("error: could not find PROTOCOL_VERSION declaration")
    return int(m.group(1))


def main() -> int:
    tags = run("git", "tag", "--list", "v*", "--sort=-v:refname").split()
    if not tags:
        print("wire-bump guard: no v* tags found — cannot establish a baseline", file=sys.stderr)
        return 2
    base = tags[0]

    try:
        old_golden, old_lib = at_tag(base, GOLDEN), at_tag(base, LIB)
    except subprocess.CalledProcessError:
        print(f"wire-bump guard: {GOLDEN} or {LIB} missing at {base} — skipping (new layout)")
        return 0

    with open(GOLDEN, encoding="utf-8") as f:
        new_golden = f.read()
    with open(LIB, encoding="utf-8") as f:
        new_lib = f.read()

    old_bytes, new_bytes = golden_bytes(old_golden), golden_bytes(new_golden)
    old_ver, new_ver = protocol_version(old_lib), protocol_version(new_lib)

    if old_bytes == new_bytes:
        print(f"wire-bump guard: golden vectors unchanged since {base} (PROTOCOL_VERSION {new_ver}) — ok")
        return 0
    if new_ver != old_ver:
        print(
            f"wire-bump guard: golden vectors changed since {base} and PROTOCOL_VERSION "
            f"bumped {old_ver} -> {new_ver} — ok (remember: re-pin firmware, cli AND hil)"
        )
        return 0

    print(
        f"wire-bump guard FAILED: the golden vectors in {GOLDEN} changed relative to {base}, "
        f"but PROTOCOL_VERSION is still {new_ver}.\n"
        "A golden change means the bytes on the wire changed. Bump PROTOCOL_VERSION in "
        "src/lib.rs (and the crate version + tag), or revert the wire change.\n"
        "Do NOT 'just fix the bytes' — see CLAUDE.md, 'Releasing a change'.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
