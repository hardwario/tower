#!/usr/bin/env python3
"""Verify the tower-protocol lockstep across the LOCAL submodule working trees.

The wire format (`tower-protocol`) is postcard, which is NOT self-describing: consumers that
pin different protocol tags do not error — they silently mis-decode bytes. This is the golden
rule of the TOWER ecosystem (see CLAUDE.md), and this script is its deterministic check,
run against the working trees actually checked out here (what `/pin` would freeze):

    manifests (tag pins):       firmware/Cargo.toml, firmware/crates/tower-kv/Cargo.toml,
                                cli/Cargo.toml, hil/Cargo.toml
    lockfiles (resolved SHAs):  firmware/Cargo.lock, cli/Cargo.lock, hil/Cargo.lock

Tag strings agreeing is necessary but NOT sufficient: a re-cut tag (same name, new commit —
it has happened, 2026-07-02) makes two repos build DIFFERENT code while every tag check says
"aligned". Comparing the lockfiles' resolved SHAs catches that drift.

Also printed (informational, never fails the check): the protocol crate version checked out in
`protocol/`, and the jolt tag pinned by its consumers (cli, hil) — a jolt mismatch is a compile
error, not an interop hazard, so it is only flagged.

Exit status: 0 = lockstep holds; non-zero = a genuine break (with instructions).
The `/lockstep` command wraps this; `/sync` and `/pin` run it as their gate.

The per-repo CI half lives in tower-firmware (tools/protocol_pin_check.py), which fetches the
tower-cli and tower-hil pins from GitHub — that guards pushes; this guards the local trees.
"""

import os
import re
import sys

# The tower-protocol pins that MUST all carry the same tag (relative to the control-plane root).
MANIFESTS = [
    "firmware/Cargo.toml",
    "firmware/crates/tower-kv/Cargo.toml",
    "cli/Cargo.toml",
    "hil/Cargo.toml",
]

# Lockfiles recording the resolved tower-protocol commit (firmware's workspace lock covers the
# root crate + tower-kv).
LOCKFILES = [
    "firmware/Cargo.lock",
    "cli/Cargo.lock",
    "hil/Cargo.lock",
]

# The jolt consumers (informational — a mismatch is a compile error in the consumer, not silent).
JOLT_MANIFESTS = ["cli/Cargo.toml", "hil/Cargo.toml"]

# Match e.g.:  tower-protocol = { git = "...", tag = "v1.1.0", features = [...] }
_PIN = re.compile(r"""tower-protocol\s*=\s*\{[^}]*?\btag\s*=\s*"([^"]+)"[^}]*\}""")
_JOLT = re.compile(r"""^jolt\s*=\s*\{[^}]*?\btag\s*=\s*"([^"]+)"[^}]*\}""", re.MULTILINE)

# The resolved source line in a Cargo.lock [[package]] block for tower-protocol.
_LOCK = re.compile(
    r'name\s*=\s*"tower-protocol"\s*\n'
    r'version\s*=\s*"[^"]*"\s*\n'
    r'source\s*=\s*"git\+[^"?]*(?:\?tag=([^#"]+))?#([0-9a-fA-F]{7,40})"'
)

# The protocol crate's own version (protocol/Cargo.toml [package] block) — the tag vX.Y.Z that
# SHOULD exist for it.
_VERSION = re.compile(r'^version\s*=\s*"([^"]+)"', re.MULTILINE)


def root() -> str:
    # tools/check_lockstep.py -> control-plane root is the parent of tools/.
    return os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def read(rel: str) -> str:
    path = os.path.join(root(), rel)
    try:
        with open(path, encoding="utf-8") as f:
            return f.read()
    except FileNotFoundError:
        sys.exit(
            f"lockstep: missing {rel} — is the submodule initialized? "
            "(git submodule update --init, or /bootstrap)"
        )


def main() -> None:
    # 1. Tag pins across all consumer manifests.
    tags = {}
    for rel in MANIFESTS:
        m = _PIN.search(read(rel))
        if not m:
            sys.exit(f"lockstep: no `tower-protocol = {{ ... tag = \"...\" }}` found in {rel}")
        tags[rel] = m.group(1)
        print(f"  {tags[rel]}  {rel}")
    if len(set(tags.values())) != 1:
        sys.exit(
            f"ERROR: tower-protocol tag MISMATCH across consumers ({sorted(set(tags.values()))}). "
            "postcard is not self-describing — a mismatch silently mis-decodes the wire. "
            "Bump every consumer in the same change-set (see the CLAUDE.md runbook)."
        )
    tag = tags[MANIFESTS[0]]
    print(f"tower-protocol pin: {tag} (all {len(MANIFESTS)} manifests agree)")

    # 2. Resolved SHAs across the lockfiles (catches the re-cut-tag / stale-git-cache drift).
    shas = {}
    for rel in LOCKFILES:
        got = _LOCK.search(read(rel))
        if not got:
            print(f"  (note: no resolved tower-protocol source in {rel} — skipping)")
            continue
        shas[rel] = got.group(2)
        print(f"  {shas[rel]}  {rel}")
    if len(set(shas.values())) > 1:
        sys.exit(
            "ERROR: tower-protocol RESOLVED-SHA mismatch across lockfiles "
            f"({ {rel: s[:12] for rel, s in shas.items()} }) although every manifest labels "
            f"{tag}. Re-cut tag or stale cargo git cache — run `cargo update -p tower-protocol` "
            "in each consumer so all locks resolve to the same commit."
        )
    if shas:
        print(f"tower-protocol resolved SHA: {next(iter(shas.values()))} (all locks agree)")

    # 3. Informational: does the checked-out protocol/ working tree match the pinned tag's
    #    version? (The tag may legitimately lag while protocol is mid-development.)
    m = _VERSION.search(read("protocol/Cargo.toml"))
    if m:
        checked_out = f"v{m.group(1)}"
        note = "matches the pin" if checked_out == tag else f"pin is {tag} — upgrade available?"
        print(f"protocol/ working tree version: {checked_out} ({note})")

    # 4. Informational: the jolt pins (cli + hil). A mismatch is a compile error in the consumer,
    #    not a silent-interop hazard, so it never fails this check.
    jolt = {rel: m.group(1) for rel in JOLT_MANIFESTS if (m := _JOLT.search(read(rel)))}
    for rel, t in jolt.items():
        print(f"  jolt {t}  {rel}")
    if len(set(jolt.values())) > 1:
        print("note: jolt tags differ between consumers (compile-error class, not silent).")

    print("lockstep OK")


if __name__ == "__main__":
    main()
