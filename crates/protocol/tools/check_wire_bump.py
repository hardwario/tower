#!/usr/bin/env python3
"""Wire-bump guard: golden vectors must not change without a version bump.

postcard is not self-describing, so the golden vectors are the byte-level definition
of a wire format. The `test` CI job guarantees the goldens match the code; this script
closes the other half of the loop: if a golden changed relative to the baseline (by
default the latest release tag), the corresponding version constant must have been
bumped too. Together the two checks make "wire change => version bump" mechanical
instead of a comment-enforced convention.

Two independent wire surfaces are guarded, each against its own version constant
(the console framing and the radio application schema evolve separately; the radio
schema rides opaquely through the gateway, so bumping it must not force a console
PROTOCOL_VERSION bump, and vice versa):

  tests/golden.rs       <-> PROTOCOL_VERSION       in src/lib.rs
  tests/radio_golden.rs <-> RADIO_SCHEMA_VERSION   in src/radio.rs

How a change is detected: per golden, not per file
--------------------------------------------------
Each golden file is split into its top-level items (`fn`, `const`, `mod`, ...). An item
holding at least one hex literal is a *golden*. Its fingerprint is the ordered list of
its integer literals (hex or decimal, by value, sign included), string literals,
`true`/`false`/`Some`/`None`, and `Type::Variant` paths. Comments, whitespace, line
wrapping and assertion messages are ignored; field NAMES are ignored too, because a
rename is wire-transparent (postcard is positional) and must not demand a bump.

Relative to the baseline, for each surface:

  * every golden that existed there must still exist under the same name with an
    identical fingerprint. A modified, renamed or removed golden (or a renamed or
    removed golden FILE, or a golden that lost its hex literals) is a change;
  * brand-new goldens are additive coverage and always pass;
  * a change passes only if the surface's version constant strictly INCREASED;
    a version DECREASE fails outright, change or not.

Why per-golden fingerprints and not "the old bytes still appear somewhere in the file"
(the pre-1.5.1 rule): appending a field to a raw-postcard golden kept the old bytes as a
prefix and passed; a modified vector passed if a new sibling repeated its old bytes; and
a variant-order pin written as `buf[0] == i as u8` carried no literal at all, so
reordering the enum plus the test's array passed. The fingerprint pairs each variant path
with its hex index, so "fixing" a pin either way changes it.

The set of surfaces checked is this script's SURFACES *united with the baseline's*
(parsed from the baseline copy of this file), so dropping an entry from SURFACES cannot
silence the guard for a golden file that existed at the baseline.

Usage (from the repo root):
  python3 tools/check_wire_bump.py              # baseline = latest v* tag (CI; needs tags)
  python3 tools/check_wire_bump.py --base REF   # baseline = any commit-ish (HEAD, origin/main, v1.5.0)
  python3 tools/check_wire_bump.py --self-test  # run the guard against synthetic repos
Exit codes: 0 ok, 1 violation, 2 cannot determine baseline.
"""

import argparse
import contextlib
import io
import os
import re
import shutil
import subprocess
import sys
import tempfile

# (golden file, version constant, file declaring the constant)
SURFACES = [
    ("tests/golden.rs", "PROTOCOL_VERSION", "src/lib.rs"),
    ("tests/radio_golden.rs", "RADIO_SCHEMA_VERSION", "src/radio.rs"),
]

# Where this script lives in the repo; the baseline's copy is parsed for its SURFACES.
GUARD_PATH = "tools/check_wire_bump.py"


# --- git / file access -----------------------------------------------------------------


def git(repo: str, *args: str) -> str:
    return subprocess.run(("git", *args), cwd=repo, check=True, capture_output=True, text=True).stdout


def at_ref(repo: str, ref: str, path: str) -> str | None:
    """`path` as of `ref`, or None if it does not exist there."""
    r = subprocess.run(("git", "show", f"{ref}:{path}"), cwd=repo, capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else None


def in_tree(repo: str, path: str) -> str | None:
    try:
        with open(os.path.join(repo, path), encoding="utf-8") as f:
            return f.read()
    except FileNotFoundError:
        return None


def version_of(source: str | None, const: str) -> int | None:
    if source is None:
        return None
    m = re.search(rf"pub const {const}:\s*u8\s*=\s*(\d+)", source)
    return int(m.group(1)) if m else None


def surfaces_at(repo: str, ref: str) -> list[tuple[str, str, str]]:
    """The SURFACES list of the baseline's copy of this script ([] if absent/unparsable)."""
    src = at_ref(repo, ref, GUARD_PATH)
    m = re.search(r"^SURFACES\s*=\s*\[(.*?)^\]", src or "", re.S | re.M)
    if not m:
        return []
    return re.findall(r'\(\s*"([^"]+)"\s*,\s*"(\w+)"\s*,\s*"([^"]+)"\s*,?\s*\)', m.group(1))


# --- a small Rust tokenizer ------------------------------------------------------------

_TOKEN = re.compile(
    r"""
      (?P<ws>\s+)
    | (?P<line_comment>//[^\n]*)
    | (?P<block_comment>/\*)
    | (?P<raw_str>b?r\#*")
    | (?P<str>b?")
    | (?P<char>b?'(?:[^'\\\n]|\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]{1,8}\}|.))')
    | (?P<lifetime>'[A-Za-z_]\w*)
    | (?P<num>(?:0[xX][0-9a-fA-F_]+|0[oO][0-7_]+|0[bB][01_]+|[0-9][0-9_]*)(?:[iu](?:8|16|32|64|128|size))?)
    | (?P<ident>[A-Za-z_]\w*)
    | (?P<punct>::|=>|.)
    """,
    re.VERBOSE | re.DOTALL,
)


def tokenize(src: str) -> list[tuple[str, str]]:
    """(kind, text) tokens with comments, whitespace and lifetimes dropped. Good enough for
    test files: strings (incl. raw/byte), chars, nested block comments, integer literals."""
    toks, i, n = [], 0, len(src)
    while i < n:
        m = _TOKEN.match(src, i)
        kind, text = m.lastgroup, m.group()
        i = m.end()
        if kind in ("ws", "line_comment", "lifetime"):
            continue
        if kind == "block_comment":
            depth = 1
            while depth and i < n:
                if src.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif src.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
            continue
        if kind == "raw_str":
            close = '"' + "#" * text.count("#")
            end = src.find(close, i)
            end = n if end < 0 else end
            toks.append(("str", src[i:end]))
            i = end + len(close)
            continue
        if kind == "str":
            j = i
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            toks.append(("str", src[i:j]))
            i = j + 1
            continue
        toks.append((kind, text))
    return toks


ITEM_KEYWORDS = {"fn", "const", "static", "mod", "struct", "enum", "union", "trait", "impl", "type", "macro_rules"}


def split_items(toks: list[tuple[str, str]]) -> dict[str, list[tuple[str, str]]]:
    """Top-level items keyed `"<keyword> <name>"` (e.g. `fn golden_hello`); tokens outside any
    item (use statements, attributes) collect under `<top-level>`."""
    items: dict[str, list] = {"<top-level>": []}
    key, depth = None, 0
    for idx, (kind, text) in enumerate(toks):
        if key is None and depth == 0 and kind == "ident" and text in ITEM_KEYWORDS:
            kw, j = text, idx + 1
            if kw == "const" and toks[j:j + 1] == [("ident", "fn")]:  # `const fn name`
                kw, j = "fn", j + 1
            if kw == "macro_rules":
                j += 1  # skip the `!`
            name = toks[j][1] if j < len(toks) and toks[j][0] == "ident" else "?"
            key = f"{kw} {name}"
            while key in items:  # e.g. two `impl X` blocks
                key += "'"
            items[key] = []
        items[key if key is not None else "<top-level>"].append((kind, text))
        if kind != "punct":
            continue
        if text in "([{":
            depth += 1
        elif text in ")]}":
            depth -= 1
            if key is not None and depth == 0 and text == "}":
                key = None
        elif text == ";" and depth == 0:
            key = None
    return items


# Macros / methods whose trailing arguments are a human message, not data: the index of the
# first message argument. Rewording an assertion message is not a wire change.
MSG_MACROS = {
    "assert": 1, "debug_assert": 1, "assert_eq": 2, "assert_ne": 2, "debug_assert_eq": 2,
    "debug_assert_ne": 2, "panic": 0, "unreachable": 0, "todo": 0, "unimplemented": 0,
}
MSG_METHODS = {"expect": 0, "expect_err": 0}
DATA_IDENTS = {"true", "false", "Some", "None"}
CAMEL = re.compile(r"[A-Z]\w*$")


def fingerprint(item: list[tuple[str, str]]) -> tuple[list[tuple[str, object]], bool]:
    """(ordered data tokens, holds-a-hex-literal) for one item; see the module doc."""
    fp: list[tuple[str, object]] = []
    has_hex = False
    stack: list[list[int]] = []  # [depth of the call's open paren, commas seen, first msg arg]
    depth = 0
    for idx, (kind, text) in enumerate(item):
        prev = item[idx - 1] if idx >= 1 else None
        prev2 = item[idx - 2] if idx >= 2 else None
        if kind == "punct" and text in "([{":
            depth += 1
            if prev == ("punct", "!") and prev2 and prev2[0] == "ident" and prev2[1] in MSG_MACROS:
                stack.append([depth, 0, MSG_MACROS[prev2[1]]])
            elif text == "(" and prev and prev[0] == "ident" and prev[1] in MSG_METHODS and prev2 == ("punct", "."):
                stack.append([depth, 0, MSG_METHODS[prev[1]]])
            continue
        if kind == "punct" and text in ")]}":
            if stack and stack[-1][0] == depth:
                stack.pop()
            depth -= 1
            continue
        if kind == "punct" and text == "," and stack and stack[-1][0] == depth:
            stack[-1][1] += 1
            continue
        if kind == "num" and text[:2] in ("0x", "0X"):
            has_hex = True
        if any(commas >= first for _, commas, first in stack):
            continue  # inside an assertion / expect message
        if kind == "num":
            digits = re.sub(r"[iu](8|16|32|64|128|size)$", "", text).replace("_", "")
            value = int(digits, 0) if digits[:2].lower() in ("0x", "0o", "0b") else int(digits)
            unary = prev == ("punct", "-") and (prev2 is None or (prev2[0] == "punct" and prev2[1] not in ")]}"))
            fp.append(("int", -value if unary else value))
        elif kind == "str":
            fp.append(("str", text))
        elif kind == "char":
            fp.append(("char", text))
        elif kind == "ident" and text in DATA_IDENTS:
            fp.append(("kw", text))
        elif (
            kind == "ident" and CAMEL.match(text)
            and item[idx + 1:idx + 2] == [("punct", "::")]
            and idx + 2 < len(item) and item[idx + 2][0] == "ident" and CAMEL.match(item[idx + 2][1])
        ):
            fp.append(("path", f"{text}::{item[idx + 2][1]}"))
    return fp, has_hex


def goldens(source: str) -> dict[str, tuple[list, bool]]:
    """Every top-level item of a golden file -> (fingerprint, holds-a-hex-literal)."""
    return {key: fingerprint(toks) for key, toks in split_items(tokenize(source)).items()}


# --- the check ---------------------------------------------------------------------------


def check_surface(repo: str, base: str, golden: str, const: str, decl: str, listed: bool) -> int:
    old_ver = version_of(at_ref(repo, base, decl), const)
    new_ver = version_of(in_tree(repo, decl), const)
    old_src, new_src = at_ref(repo, base, golden), in_tree(repo, golden)

    if old_ver is None:
        print(f"wire-bump guard: {golden}: {const} not declared at {base}: new surface, nothing to compare")
        return 0
    if new_ver is None:
        if not listed and new_src is None:
            # Listed at the baseline only, its constant AND golden file gone: a deliberately
            # retired surface (removing the constant breaks every consumer at compile time).
            print(f"wire-bump guard: {golden}: surface retired since {base} ({const} removed), not guarded")
            return 0
        print(f"wire-bump guard FAILED: cannot find `pub const {const}: u8 = N` in {decl}", file=sys.stderr)
        return 1
    if new_ver < old_ver:
        print(
            f"wire-bump guard FAILED: {const} DECREASED {old_ver} -> {new_ver} relative to {base}.\n"
            "A version number never goes back: an old value would alias a wire the field has\n"
            "already shipped. Restore it (or bump past it).",
            file=sys.stderr,
        )
        return 1
    bumped = new_ver > old_ver

    old = {k: fp for k, (fp, hexy) in goldens(old_src).items() if hexy} if old_src is not None else {}
    changes: list[str] = []
    if new_src is None:
        if old_src is None and listed:
            print(f"wire-bump guard FAILED: golden file {golden} is missing", file=sys.stderr)
            return 1
        if old:
            changes.append(f"{golden}: golden file removed or renamed ({len(old)} golden(s) gone)")
        added = 0
    else:
        new = goldens(new_src)
        for key, fp in old.items():
            if key not in new:
                changes.append(f"{key}: removed or renamed")
            elif new[key][0] != fp:
                changes.append(f"{key}: modified")
            elif not new[key][1]:
                changes.append(f"{key}: no longer a golden (its hex literals are gone)")
        added = sum(1 for key, (_, hexy) in new.items() if hexy and key not in old)

    if not changes:
        note = f", +{added} new golden(s): additive coverage" if added else ""
        print(f"wire-bump guard: {golden}: {len(old)} golden(s) intact since {base} ({const} {new_ver}{note}): ok")
        return 0
    if bumped:
        print(
            f"wire-bump guard: {golden}: {len(changes)} golden(s) changed since {base} and {const} "
            f"bumped {old_ver} -> {new_ver}: ok (remember: re-pin the consumers)"
        )
        return 0
    listing = "\n".join(f"  - {c}" for c in changes)
    print(
        f"wire-bump guard FAILED: golden vectors in {golden} changed relative to {base}, "
        f"but {const} is still {new_ver}:\n{listing}\n"
        f"A golden change means the bytes on the wire changed. Bump {const} in "
        f"{decl} (and the crate version + tag), or revert the wire change.\n"
        "Do NOT 'just fix the bytes'; see AGENTS.md, 'Releasing a change'. (Adding coverage? "
        "Add a NEW golden fn instead of editing an existing one.)",
        file=sys.stderr,
    )
    return 1


def check(repo: str, base: str, surfaces: list[tuple[str, str, str]]) -> int:
    merged = {g: (c, d) for g, c, d in surfaces_at(repo, base)}
    merged.update({g: (c, d) for g, c, d in surfaces})  # the current definition wins
    listed = {g for g, _, _ in surfaces}
    worst = 0
    for golden, (const, decl) in merged.items():
        worst = max(worst, check_surface(repo, base, golden, const, decl, golden in listed))
    return worst


def resolve_base(repo: str, base: str | None) -> str | None:
    if base is None:
        tags = git(repo, "tag", "--list", "v*", "--sort=-v:refname").split()
        return tags[0] if tags else None
    r = subprocess.run(
        ("git", "rev-parse", "--verify", "--quiet", f"{base}^{{commit}}"), cwd=repo, capture_output=True, text=True
    )
    return base if r.returncode == 0 else None


# --- self-test -----------------------------------------------------------------------------

_FIXTURE = {
    "src/lib.rs": "pub const PROTOCOL_VERSION: u8 = 3;\n",
    "src/radio.rs": "pub const RADIO_SCHEMA_VERSION: u8 = 2;\n",
    "tests/golden.rs": """use x::*;

fn frame<T>(mt: MsgType, seq: u16, p: &T) -> Vec<u8> { todo!() }

#[test]
fn golden_hello() {
    // the bytes below (e.g. 0xdead in a comment) are not data
    let got = frame(MsgType::Hello, 1, &Hello { name: "app", version: "fw", session_id: 1 });
    assert_eq!(got, [0x03, 0x60, 0x01, 0x0e], "hello bytes");
}

#[test]
fn golden_log() {
    let got = frame(MsgType::Log, 7, &Log { level: Level::Warn, uptime_us: 0x0102_0304, rssi: -67 });
    assert_eq!(got, [0x03, 0x61, 0x07, 0x0f]);
}

#[test]
fn level_variant_order_is_stable() {
    let pins: [(Level, u8); 3] = [(Level::Error, 0x00), (Level::Warn, 0x01), (Level::Info, 0x02)];
    for (level, byte) in pins {
        assert_eq!(rec(&level), [byte], "Level order");
    }
}

#[test]
fn roundtrip_only() {
    assert!(decode(&[1, 2]).is_ok());
}
""",
    "tests/radio_golden.rs": """#[test]
fn golden_node_button() {
    let got = enc_msg(&NodeMsg::Button { kind: ButtonKind::Click, count: 42 });
    assert_eq!(got, [0x02, 0x01, 0x02, 0x2a]);
}

#[test]
fn golden_node_shell() {
    let got = enc_msg(&NodeMsg::Shell(NodeShellChunk { cmd_id: 9, result: 3, chunk: 5, last: true, text: "ok" }));
    assert_eq!(got, [0x02, 0x04, 0x09, 0x03, 0x05, 0x01, 0x02, 0x6f, 0x6b]);
}
""",
}

_NEW_FN = "\n#[test]\nfn golden_extra() {\n    assert_eq!(enc(&Extra::A { v: 5 }), [0x05, 0x00]);\n}\n"


def _edit(repo: str, path: str, old: str, new: str) -> None:
    src = in_tree(repo, path)
    assert src is not None and old in src, f"self-test fixture: {old!r} not in {path}"
    with open(os.path.join(repo, path), "w", encoding="utf-8") as f:
        f.write(src.replace(old, new, 1))


def _append(repo: str, path: str, text: str) -> None:
    with open(os.path.join(repo, path), "a", encoding="utf-8") as f:
        f.write(text)


def _surfaces_without_radio():
    return [s for s in SURFACES if s[0] != "tests/radio_golden.rs"]


# (name, expected exit code, mutation(repo) -> optional replacement SURFACES)
_SCENARIOS = [
    ("baseline unchanged", 0, lambda r: None),
    ("brand-new golden fn added", 0, lambda r: _append(r, "tests/golden.rs", _NEW_FN)),
    ("reflow + comment + assert-message edits only", 0, lambda r: (
        _edit(r, "tests/golden.rs", "[0x03, 0x61, 0x07, 0x0f]", "[\n        0x03, 0x61,\n        0x07, 0x0f,\n    ]"),
        _edit(r, "tests/golden.rs", "0xdead", "0xbeef"),
        _edit(r, "tests/golden.rs", '"hello bytes"', '"Hello wire bytes changed"'),
    )),
    ("field rename only (wire-transparent)", 0, lambda r: _edit(r, "tests/golden.rs", "uptime_us:", "uptime:")),
    ("version bumped, no golden change", 0, lambda r: _edit(r, "src/lib.rs", "= 3;", "= 4;")),
    ("golden modified + its version bumped", 0, lambda r: (
        _edit(r, "tests/radio_golden.rs", "0x2a]", "0x2b]"),
        _edit(r, "src/radio.rs", "= 2;", "= 3;"),
    )),
    ("new surface (constant absent at base)", 0, lambda r: (
        _append(r, "src/lib.rs", "pub const EXTRA_VERSION: u8 = 1;\n"),
        _append(r, "tests/extra_golden.rs", _NEW_FN),
    ) and SURFACES + [("tests/extra_golden.rs", "EXTRA_VERSION", "src/lib.rs")]),
    ("field APPENDED to a radio golden", 1, lambda r: _edit(r, "tests/radio_golden.rs", "0x02, 0x2a]", "0x02, 0x2a, 0x00]")),
    ("variant pin: bytes reordered", 1, lambda r: _edit(r, "tests/golden.rs", "0x00), (Level::Warn, 0x01)", "0x01), (Level::Warn, 0x00)")),
    ("variant pin: variants reordered", 1, lambda r: _edit(r, "tests/golden.rs", "(Level::Error, 0x00), (Level::Warn,", "(Level::Warn, 0x00), (Level::Error,")),
    ("golden modified, new sibling repeats its old bytes", 1, lambda r: (
        _edit(r, "tests/golden.rs", "[0x03, 0x61, 0x07, 0x0f]", "[0x03, 0x61, 0x07, 0x10]"),
        _append(r, "tests/golden.rs", "\n#[test]\nfn golden_log_old() {\n    assert_eq!(x, [0x03, 0x61, 0x07, 0x0f]);\n}\n"),
    )),
    ("golden input values swapped", 1, lambda r: _edit(r, "tests/golden.rs", 'name: "app", version: "fw"', 'name: "fw", version: "app"')),
    ("golden input sign flipped", 1, lambda r: _edit(r, "tests/golden.rs", "rssi: -67", "rssi: 67")),
    ("golden fn renamed", 1, lambda r: _edit(r, "tests/golden.rs", "fn golden_log()", "fn golden_log_v2()")),
    ("golden fn removed", 1, lambda r: _edit(r, "tests/radio_golden.rs", "fn golden_node_shell()", "fn _gone() {}\nfn _unused()")),
    ("golden rewritten in decimal (no hex left)", 1, lambda r: _edit(r, "tests/radio_golden.rs", "[0x02, 0x01, 0x02, 0x2a]", "[2, 1, 2, 42]")),
    ("golden file renamed (SURFACES updated)", 1, lambda r: (
        os.rename(os.path.join(r, "tests/golden.rs"), os.path.join(r, "tests/console_golden.rs")),
    ) and [("tests/console_golden.rs", "PROTOCOL_VERSION", "src/lib.rs"), SURFACES[1]]),
    ("golden file deleted + dropped from SURFACES", 1, lambda r: (
        os.remove(os.path.join(r, "tests/radio_golden.rs")),
    ) and _surfaces_without_radio()),
    ("version DECREASED (goldens unchanged)", 1, lambda r: _edit(r, "src/lib.rs", "= 3;", "= 2;")),
    ("radio golden modified, only PROTOCOL_VERSION bumped", 1, lambda r: (
        _edit(r, "tests/radio_golden.rs", "0x2a]", "0x2b]"),
        _edit(r, "src/lib.rs", "= 3;", "= 4;"),
    )),
]


def self_test() -> int:
    for var in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
        os.environ.pop(var, None)
    quiet = ("-c", "user.name=guard", "-c", "user.email=guard@localhost", "-c", "commit.gpgsign=false")
    failures = 0
    with tempfile.TemporaryDirectory(prefix="wire-bump-guard-") as repo:
        for path, text in _FIXTURE.items():
            os.makedirs(os.path.dirname(os.path.join(repo, path)), exist_ok=True)
            with open(os.path.join(repo, path), "w", encoding="utf-8") as f:
                f.write(text)
        os.makedirs(os.path.join(repo, "tools"), exist_ok=True)
        shutil.copy(os.path.abspath(__file__), os.path.join(repo, GUARD_PATH))  # baseline SURFACES
        git(repo, "init", "-q")
        git(repo, "add", "-A")
        git(repo, *quiet, "commit", "-q", "--no-verify", "-m", "fixture")
        git(repo, "tag", "v1.0.0")
        base = resolve_base(repo, None)
        assert base == "v1.0.0", base
        for name, want, mutate in _SCENARIOS:
            git(repo, "checkout", "-q", "--", ".")
            git(repo, "clean", "-qfd")
            surfaces = mutate(repo)
            surfaces = surfaces if isinstance(surfaces, list) else SURFACES
            out = io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
                got = check(repo, base, surfaces)
            verdict = "FAIL" if got else "pass"
            ok = got == want
            failures += not ok
            print(f"  {'ok ' if ok else 'BAD'}  guard says {verdict:4}  {name}")
            if not ok:
                print("       " + out.getvalue().replace("\n", "\n       "))
    print(f"self-test: {len(_SCENARIOS) - failures}/{len(_SCENARIOS)} scenarios behaved as expected")
    return 1 if failures else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    ap.add_argument("--base", help="baseline commit-ish (default: the latest v* tag)")
    ap.add_argument("--self-test", action="store_true", help="exercise the guard on synthetic repos")
    args = ap.parse_args()
    if args.self_test:
        return self_test()
    base = resolve_base(".", args.base)
    if base is None:
        what = f"ref {args.base!r} not found" if args.base else "no v* tags found"
        print(f"wire-bump guard: {what}; cannot establish a baseline", file=sys.stderr)
        return 2
    return check(".", base, SURFACES)


if __name__ == "__main__":
    sys.exit(main())
