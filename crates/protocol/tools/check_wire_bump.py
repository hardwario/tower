#!/usr/bin/env python3
"""Wire-bump guard: golden vectors must not change without a version bump.

postcard is not self-describing, so the golden vectors are the byte-level definition
of a wire format. The `test` CI job guarantees the goldens match the code; this script
closes the other half of the loop: if golden BYTES changed relative to the latest
release tag, the corresponding version constant must have changed too. Together the
two checks make "wire change => version bump" mechanical instead of a comment-enforced
convention.

Two independent wire surfaces are guarded, each against its own version constant
(the console framing and the radio application schema evolve separately — the radio
schema rides opaquely through the gateway, so bumping it must not force a console
PROTOCOL_VERSION bump, and vice versa):

  tests/golden.rs       <-> PROTOCOL_VERSION       in src/lib.rs
  tests/radio_golden.rs <-> RADIO_SCHEMA_VERSION   in src/radio.rs

Usage: python3 tools/check_wire_bump.py   (run from the repo root; needs tags fetched)
Exit codes: 0 ok, 1 violation, 2 cannot determine baseline.
"""

import re
import subprocess
import sys

# (golden file, version constant, file declaring the constant)
SURFACES = [
    ("tests/golden.rs", "PROTOCOL_VERSION", "src/lib.rs"),
    ("tests/radio_golden.rs", "RADIO_SCHEMA_VERSION", "src/radio.rs"),
]

HEX = re.compile(r"0x[0-9a-fA-F_]+")


def run(*args: str) -> str:
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def at_tag(tag: str, path: str) -> str:
    return run("git", "show", f"{tag}:{path}")


def golden_bytes(source: str) -> list[str]:
    # Every hex literal in file order. This covers the expected wire bytes AND the
    # test inputs that produce them — a change to either is a golden change.
    return HEX.findall(source)


def version_of(source: str, const: str) -> int:
    m = re.search(rf"pub const {const}:\s*u8\s*=\s*(\d+)", source)
    if not m:
        sys.exit(f"error: could not find {const} declaration")
    return int(m.group(1))


def is_subsequence(needle: list[str], hay: list[str]) -> bool:
    # Additive-coverage friendly: NEW golden tests may append hex literals, so the rule
    # is "every old byte still present, in order" (subsequence), not strict equality.
    # Any change to an EXISTING vector still trips this: content changes move that
    # frame's CRC/length bytes, so a modified vector cannot preserve the old sequence.
    it = iter(hay)
    return all(tok in it for tok in needle)


def check_surface(base: str, golden: str, const: str, decl: str) -> int:
    try:
        old_golden, old_decl = at_tag(base, golden), at_tag(base, decl)
    except subprocess.CalledProcessError:
        print(f"wire-bump guard: {golden} or {decl} missing at {base} — skipping (new surface)")
        return 0

    with open(golden, encoding="utf-8") as f:
        new_golden = f.read()
    with open(decl, encoding="utf-8") as f:
        new_decl = f.read()

    old_bytes, new_bytes = golden_bytes(old_golden), golden_bytes(new_golden)
    old_ver, new_ver = version_of(old_decl, const), version_of(new_decl, const)

    if is_subsequence(old_bytes, new_bytes):
        extra = len(new_bytes) - len(old_bytes)
        note = f" (+{extra} new literal(s) — additive coverage)" if extra else ""
        print(f"wire-bump guard: {golden} intact since {base} ({const} {new_ver}){note} — ok")
        return 0
    if new_ver != old_ver:
        print(
            f"wire-bump guard: {golden} changed since {base} and {const} "
            f"bumped {old_ver} -> {new_ver} — ok (remember: re-pin the consumers)"
        )
        return 0

    print(
        f"wire-bump guard FAILED: the golden vectors in {golden} changed relative to {base}, "
        f"but {const} is still {new_ver}.\n"
        f"A golden change means the bytes on the wire changed. Bump {const} in "
        f"{decl} (and the crate version + tag), or revert the wire change.\n"
        "Do NOT 'just fix the bytes' — see CLAUDE.md, 'Releasing a change'.",
        file=sys.stderr,
    )
    return 1


def main() -> int:
    tags = run("git", "tag", "--list", "v*", "--sort=-v:refname").split()
    if not tags:
        print("wire-bump guard: no v* tags found — cannot establish a baseline", file=sys.stderr)
        return 2
    base = tags[0]

    worst = 0
    for golden, const, decl in SURFACES:
        worst = max(worst, check_surface(base, golden, const, decl))
    return worst


if __name__ == "__main__":
    sys.exit(main())
