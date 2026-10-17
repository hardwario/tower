#!/usr/bin/env python3
"""Verify the tower-protocol pin is identical everywhere (the golden lockstep rule).

The console wire format lives in the separate `tower-protocol` repo and is pinned BY GIT TAG by
every consumer: this repo (tower-firmware), tower-cli and tower-hil. postcard is NOT
self-describing, so a mismatch does not error at build time; it silently mis-decodes bytes on
the wire. This guard turns that latent hazard into a hard failure.

This is the SHARED CI implementation of the check: tower-firmware's CI runs it from its own
checkout, and tower-cli's / tower-hil's CI check out the sibling repos and run THIS file from
the tower-firmware checkout, so all three gates apply identical logic. (The developer-facing
check across the LOCAL working trees is the TOWER control plane's `tools/check_lockstep.py`
/ `/lockstep`, which applies the same rules plus control-plane-only context.)

Usage:
    protocol_pin_check.py                       # this repo only (the firmware workspace)
    protocol_pin_check.py TREE [TREE ...]       # each TREE = a consumer checkout (dir holding
                                                # Cargo.toml + Cargo.lock); all must agree
    protocol_pin_check.py ... --resolve-remote  # also `git ls-remote` the tag and require the
                                                # locked SHA == the tag's current commit
    protocol_pin_check.py ... --protocol-git DIR  # same, resolving the tag in a local clone

What is checked, per tree (fail-closed: anything not positively verified is an error):
  * every Cargo.toml in the tree (a filesystem walk, so a new workspace crate can't be missed;
    `target/` and hidden dirs skipped), parsed with tomllib, so comments can't shadow a pin, and
    EVERY dependency table is inspected: [dependencies], [dev-dependencies],
    [build-dependencies], [target.*.…], [workspace.dependencies], renamed deps
    (`package = "tower-protocol"`) and `workspace = true` inheritance;
  * every tower-protocol dependency must be `{ git = <url>, tag = "<tag>" }`; `rev`, `branch`,
    `path`, or a registry version are rejected, and so is any [patch]/[replace] entry for it;
  * the tree's `.cargo/config.toml` must not override it (`paths = [...]` or [patch]): an
    error when that file is committed, a WARNING when it is an untracked local co-dev file;
  * Cargo.lock must exist and hold EXACTLY ONE tower-protocol package whose source is
    `git+<same url>?tag=<same tag>#<40-hex sha>` (a `?rev=`/`?branch=`/path source fails).
Across trees: one tag, one git URL, one resolved SHA. Tag strings agreeing is necessary but NOT
sufficient: a re-cut tag (same name, new commit; it happened 2026-07-02) makes two repos build
DIFFERENT code while every tag check says "aligned"; the SHA comparison catches that, and
--resolve-remote additionally catches a tag that moved after everyone locked it.

Plain python3 >= 3.11 (tomllib; no shell, no coreutils) so it runs the same on Linux, macOS and
Windows. Exit 0 = lockstep holds; exit 1 = a break (each problem printed as `ERROR:`).
"""

import os
import re
import subprocess
import sys
from urllib.parse import parse_qs, urlsplit

if sys.version_info < (3, 11):
    sys.exit("protocol-pin: needs Python >= 3.11 (tomllib)")

import tomllib  # noqa: E402  (after the version guard on purpose)

CRATE = "tower-protocol"
SKIP_DIRS = {"target", "node_modules", "__pycache__"}
_SHA = re.compile(r"^[0-9a-f]{40}$")
_DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


class Problems:
    def __init__(self) -> None:
        self.errors: list[str] = []

    def error(self, msg: str) -> None:
        self.errors.append(msg)
        print(f"ERROR: {msg}")


def norm_url(url: str) -> str:
    """Canonical form of a git URL for comparison (scheme/host case, `.git`, trailing `/`)."""
    url = url.strip()
    if url.endswith("/"):
        url = url[:-1]
    if url.endswith(".git"):
        url = url[:-4]
    parts = urlsplit(url)
    return f"{parts.scheme.lower()}://{parts.netloc.lower()}{parts.path}"


def load_toml(path: str, probs: Problems | None):
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except FileNotFoundError:
        return None
    except tomllib.TOMLDecodeError as e:
        if probs:
            probs.error(f"{path}: not valid TOML ({e}); cannot verify the pin")
        return None


def find_manifests(tree: str) -> list[str]:
    """Every Cargo.toml in the tree. Skips build output, hidden dirs, and nested checkouts (a
    subdirectory with its own `.git` is another repo, e.g. a sibling checked out for CI)."""
    found = []
    for dirpath, dirnames, filenames in os.walk(tree):
        dirnames[:] = sorted(
            d for d in dirnames
            if d not in SKIP_DIRS and not d.startswith(".")
            and not os.path.exists(os.path.join(dirpath, d, ".git"))
        )
        if "Cargo.toml" in filenames:
            found.append(os.path.join(dirpath, "Cargo.toml"))
    return found


def crate_deps(table) -> list:
    """(key, spec) for every entry of a dependency table that IS tower-protocol (incl. renamed)."""
    out = []
    if not isinstance(table, dict):
        return out
    for key, spec in table.items():
        if key == CRATE or (isinstance(spec, dict) and spec.get("package") == CRATE):
            out.append((key, spec))
    return out


def iter_dep_tables(doc: dict):
    """Yield (label, table) for every dependency table in a manifest."""
    for t in _DEP_TABLES:
        if t in doc:
            yield f"[{t}]", doc[t]
    for cfg, body in (doc.get("target") or {}).items():
        if isinstance(body, dict):
            for t in _DEP_TABLES:
                if t in body:
                    yield f"[target.{cfg!r}.{t}]", body[t]
    ws = doc.get("workspace")
    if isinstance(ws, dict) and "dependencies" in ws:
        yield "[workspace.dependencies]", ws["dependencies"]


def classify(spec, where: str, probs: Problems):
    """Return (url, tag) for a valid git-tag pin, else record an error and return None."""
    if not isinstance(spec, dict):
        probs.error(f"{where}: {CRATE} = {spec!r} is a registry/version requirement; it must be "
                    "pinned by git tag")
        return None
    if "path" in spec:
        probs.error(f"{where}: {CRATE} has `path = {spec['path']!r}`; a local path must never be "
                    "committed (use an uncommitted `paths` override for co-development)")
        return None
    if "git" not in spec:
        probs.error(f"{where}: {CRATE} has no `git` source; it must be pinned by git tag")
        return None
    for bad in ("rev", "branch"):
        if bad in spec:
            probs.error(f"{where}: {CRATE} is pinned by `{bad} = {spec[bad]!r}`; it must be "
                        "pinned by `tag`")
            return None
    if "tag" not in spec:
        probs.error(f"{where}: {CRATE} has a git source but no `tag` (tracks the default branch)")
        return None
    return norm_url(spec["git"]), spec["tag"]


def git_tracked(tree: str, rel: str):
    """True/False whether <tree>/<rel> is tracked by git; None if <tree> isn't a git checkout."""
    try:
        r = subprocess.run(["git", "-C", tree, "ls-files", "--error-unmatch", rel],
                           capture_output=True, text=True, timeout=60, check=False)
        if r.returncode == 0:
            return True
        r = subprocess.run(["git", "-C", tree, "rev-parse", "--is-inside-work-tree"],
                           capture_output=True, text=True, timeout=60, check=False)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return False if r.returncode == 0 else None


def check_cargo_config(tree: str, probs: Problems) -> None:
    """The tree's .cargo/config(.toml) must not redirect tower-protocol (`paths` / [patch]).

    A COMMITTED override is an error (every clone would build local sources). An untracked,
    git-ignored one is a developer's local co-dev setup (tower-cli documents exactly that),
    reported as a WARNING. Outside a git checkout we can't tell, so it counts as committed."""
    for name in ("config.toml", "config"):
        path = os.path.join(tree, ".cargo", name)
        doc = load_toml(path, probs)
        if doc is None:
            continue
        rel = os.path.relpath(path)
        hits = []
        for p in doc.get("paths") or []:
            target = os.path.join(tree, p) if not os.path.isabs(p) else p
            sub = load_toml(os.path.join(target, "Cargo.toml"), None)
            if sub is None:
                hits.append(f"`paths` override {p!r} cannot be resolved; refusing to assume it "
                            f"does not shadow {CRATE}")
            elif (sub.get("package") or {}).get("name") == CRATE:
                hits.append(f"`paths` override {p!r} shadows {CRATE} with a local checkout")
        for src, entries in (doc.get("patch") or {}).items():
            if crate_deps(entries):
                hits.append(f"[patch.{src!r}] overrides {CRATE}")
        if not hits:
            continue
        if git_tracked(tree, os.path.join(".cargo", name)) is False:
            for h in hits:
                print(f"WARNING: {rel} (untracked, local only): {h}; local builds use it instead "
                      "of the pinned tag; never commit it")
        else:
            for h in hits:
                probs.error(f"{rel} (committed): {h}")


def check_tree(tree: str, probs: Problems):
    """Return a dict {name, pins: {(url, tag)}, lock: (url, tag, sha) | None} for one checkout."""
    root_manifest = os.path.join(tree, "Cargo.toml")
    root_doc = load_toml(root_manifest, probs)
    if root_doc is None:
        probs.error(f"{tree}: no readable Cargo.toml; is this a consumer checkout?")
        return None
    pkg = (root_doc.get("package") or {}).get("name") or "?"
    name = f"{pkg} ({os.path.relpath(tree)})"
    ws_deps = dict(crate_deps(((root_doc.get("workspace") or {}).get("dependencies")) or {}))

    # Every declared workspace member must exist on disk (else the walk below can't see it).
    for member in (root_doc.get("workspace") or {}).get("members", []):
        if "*" not in member and not os.path.isfile(os.path.join(tree, member, "Cargo.toml")):
            probs.error(f"{tree}: workspace member {member!r} has no Cargo.toml")

    pins = {}
    for path in find_manifests(tree):
        doc = load_toml(path, probs)
        if doc is None:
            continue
        rel = os.path.relpath(path)
        for label, table in iter_dep_tables(doc):
            for key, spec in crate_deps(table):
                where = f"{rel} {label} {key}"
                if isinstance(spec, dict) and spec.get("workspace") is True:
                    inherited = ws_deps.get(key)
                    if inherited is None:
                        probs.error(f"{where}: `workspace = true` but the workspace root defines "
                                    f"no [workspace.dependencies] {key}")
                        continue
                    spec = inherited
                got = classify(spec, where, probs)
                if got:
                    pins[where] = got
        for src, entries in (doc.get("patch") or {}).items():
            for key, _ in crate_deps(entries):
                probs.error(f"{rel} [patch.{src!r}] {key}: overrides {CRATE}; must not be committed")
        for key in (doc.get("replace") or {}):
            if key.split(":")[0].split("@")[0] == CRATE:
                probs.error(f"{rel} [replace] {key!r}: overrides {CRATE}; must not be committed")

    check_cargo_config(tree, probs)

    for where, (url, tag) in sorted(pins.items()):
        print(f"  {tag:<10} {where}")
    if not pins:
        probs.error(f"{name}: no `{CRATE} = {{ git = …, tag = … }}` pin found in any "
                    "manifest; refusing to report OK on nothing")

    # Cargo.lock: exactly one tower-protocol package, sourced from git by ?tag=<the same tag>.
    lock_path = os.path.join(tree, "Cargo.lock")
    lock_doc = load_toml(lock_path, probs)
    lock = None
    if lock_doc is None:
        probs.error(f"{name}: {os.path.relpath(lock_path)} missing; the resolved commit cannot "
                    "be verified (every TOWER consumer commits its lockfile)")
    else:
        pkgs = [p for p in lock_doc.get("package", []) if p.get("name") == CRATE]
        rel = os.path.relpath(lock_path)
        if not pkgs:
            probs.error(f"{rel}: no {CRATE} package locked (run `cargo update -p {CRATE}`)")
        elif len(pkgs) > 1:
            probs.error(f"{rel}: {len(pkgs)} {CRATE} packages locked "
                        f"({[p.get('version') for p in pkgs]}): two protocol versions linked")
        else:
            src = pkgs[0].get("source", "")
            m = re.fullmatch(r"git\+([^?#]+)(?:\?([^#]*))?#([0-9a-fA-F]+)", src)
            if not m:
                probs.error(f"{rel}: {CRATE} source {src!r} is not a git source (path/registry "
                            "override?); the resolved commit cannot be verified")
            else:
                url, query, sha = norm_url(m.group(1)), parse_qs(m.group(2) or ""), m.group(3)
                if "tag" not in query:
                    kind = next(iter(query), "default branch")
                    probs.error(f"{rel}: {CRATE} locked by `{kind}` ({src!r}), not by tag")
                elif not _SHA.match(sha.lower()):
                    probs.error(f"{rel}: {CRATE} locked SHA {sha!r} is not a full 40-hex commit")
                else:
                    lock = (url, query["tag"][0], sha.lower())
                    print(f"  {lock[2]}  {rel} (?tag={lock[1]})")

    distinct = set(pins.values())
    if len(distinct) > 1:
        probs.error(f"{name}: {CRATE} pins DISAGREE inside the tree: {sorted(distinct)}")
    if lock and len(distinct) == 1:
        (url, tag), = distinct
        if lock[1] != tag:
            probs.error(f"{name}: Cargo.lock resolved ?tag={lock[1]} but the manifests pin {tag}: "
                        f"stale lockfile; run `cargo update -p {CRATE}`")
        if lock[0] != url:
            probs.error(f"{name}: Cargo.lock source {lock[0]} != manifest git URL {url}")
    return {"name": name, "tree": tree, "pins": distinct, "lock": lock}


def resolve_tag(url: str, tag: str, protocol_git: str | None, probs: Problems):
    """The commit the tag points to NOW (annotated tags peeled), or None (error recorded)."""
    if protocol_git:
        cmd = ["git", "-C", protocol_git, "rev-parse", "--verify", "--quiet", f"refs/tags/{tag}^{{commit}}"]
        where = f"{protocol_git} (local clone)"
    else:
        cmd = ["git", "ls-remote", "--tags", url, f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}"]
        where = url
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=60, check=False)
    except (OSError, subprocess.TimeoutExpired) as e:
        probs.error(f"could not run {' '.join(cmd)}: {e}")
        return None
    if out.returncode != 0 or not out.stdout.strip():
        probs.error(f"tag {tag} not found in {where}; consumers pin a tag that does not exist")
        return None
    if protocol_git:
        return out.stdout.strip().lower()
    refs = dict(line.split("\t")[::-1] for line in out.stdout.strip().splitlines())
    return (refs.get(f"refs/tags/{tag}^{{}}") or refs.get(f"refs/tags/{tag}", "")).lower() or None


def main(argv: list[str]) -> int:
    trees, resolve_remote, protocol_git = [], False, None
    it = iter(argv)
    for arg in it:
        if arg == "--resolve-remote":
            resolve_remote = True
        elif arg == "--protocol-git":
            protocol_git = next(it, None)
            if protocol_git is None:
                sys.exit("protocol-pin: --protocol-git needs a directory")
        elif arg.startswith("-"):
            sys.exit(f"protocol-pin: unknown option {arg!r} (see the module docstring for usage)")
        else:
            trees.append(arg)
    if not trees:
        # tools/protocol_pin_check.py -> the repo root is the parent of tools/.
        trees = [os.path.dirname(os.path.dirname(os.path.abspath(__file__)))]

    probs = Problems()
    results = []
    for tree in trees:
        print(f"{tree}:")
        r = check_tree(tree, probs)
        if r:
            results.append(r)

    tags = {t for r in results for (_, t) in r["pins"]}
    urls = {u for r in results for (u, _) in r["pins"]}
    shas = {r["lock"][2] for r in results if r["lock"]}
    if len(tags) > 1:
        probs.error(f"{CRATE} tag MISMATCH across consumers: "
                    f"{ {r['name']: sorted(t for _, t in r['pins']) for r in results} }. postcard is "
                    "not self-describing; bump every consumer in the same change-set.")
    if len(urls) > 1:
        probs.error(f"{CRATE} git URL differs across consumers: {sorted(urls)} (a fork with the "
                    "same tag name is the same silent hazard)")
    if len(shas) > 1:
        probs.error(f"{CRATE} RESOLVED-SHA mismatch: "
                    f"{ {r['name']: r['lock'][2][:12] for r in results if r['lock']} }"
                    + (" although the tags agree (re-cut tag or stale cargo git cache)"
                       if len(tags) == 1 else "")
                    + f". Run `cargo update -p {CRATE}` in each consumer so all locks resolve to "
                    "the same commit.")

    if (resolve_remote or protocol_git) and len(tags) == 1 and len(urls) == 1 and len(shas) == 1:
        (tag,), (url,), (sha,) = tags, urls, shas
        current = resolve_tag(url, tag, protocol_git, probs)
        if current:
            print(f"tag {tag} currently points at {current}")
            if current != sha:
                probs.error(f"tag {tag} now points at {current[:12]} but every lockfile resolved "
                            f"{sha[:12]}: the tag was RE-CUT after the consumers locked it; "
                            f"re-run `cargo update -p {CRATE}` everywhere (and find out why a "
                            "published tag moved).")

    if probs.errors:
        print(f"lockstep BROKEN: {len(probs.errors)} problem(s) above.")
        return 1
    (tag,), (sha,) = tags, shas
    print(f"lockstep OK: {len(results)} consumer tree(s) pin {CRATE} {tag} @ {sha}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
