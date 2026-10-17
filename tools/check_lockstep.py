#!/usr/bin/env python3
"""Verify the tower-protocol lockstep across the LOCAL submodule working trees.

The wire format (`tower-protocol`) is postcard, which is NOT self-describing: consumers that
pin different protocol tags do not error; they silently mis-decode bytes. This is the golden
rule of the TOWER ecosystem (see AGENTS.md), and this script is its deterministic check, run
against the working trees actually checked out here (exactly what `/pin` would freeze).

Usage:
    python3 tools/check_lockstep.py            # the gate (/lockstep, /sync, /pin, control-plane CI)
    python3 tools/check_lockstep.py --strict   # also FAIL on a local co-dev override (/pin)
    python3 tools/check_lockstep.py --remote   # also `git ls-remote` the tag (catches a tag
                                               # re-cut upstream after the local clone fetched it)
    python3 tools/check_lockstep.py --root DIR # check another control-plane layout (tests)

Fail-closed checks (each problem printed as `ERROR:`; exit 1 if any):
  * consumers `firmware/`, `cli/`, `hil/`: EVERY Cargo.toml in each tree (a filesystem walk,
    so a new workspace crate can't be missed) parsed with tomllib, so a commented-out line can't
    shadow the real pin and every dependency table counts ([dependencies], [dev-dependencies],
    [build-dependencies], [target.*.…], [workspace.dependencies], renamed deps, `workspace =
    true`). Each tower-protocol dependency must be `{ git = <url>, tag = "<tag>" }`; `rev`,
    `branch`, `path`, registry versions, and any [patch]/[replace] of it are errors; so is a
    `paths`/[patch] override in the tree's own `.cargo/config.toml`;
  * each consumer's Cargo.lock must hold EXACTLY ONE tower-protocol package, sourced
    `git+<same url>?tag=<same tag>#<40-hex sha>`; a `?rev=`/`?branch=`/path source, or a lock
    whose ?tag= disagrees with the manifests (stale lock), fails;
  * across consumers: one tag, one git URL, one resolved SHA (tag strings agreeing is NOT
    sufficient: a re-cut tag, same name/new commit, happened 2026-07-02);
  * the resolved SHA must equal the commit the tag points to in `protocol/`
    (`git -C protocol rev-parse <tag>^{commit}`; falls back to `git ls-remote` when the local
    clone lacks the tag, e.g. a shallow CI submodule checkout).

Informational (never fails): the `protocol/` checkout vs the pinned tag's COMMIT (exactly at it,
N commits ahead, or diverged), and the jolt pins of its consumers (cli, hil); a jolt mismatch
is a compile error, not a silent-interop hazard. A git-ignored control-plane
`.cargo/config.toml` that redirects tower-protocol is a WARNING here (local co-development is
legitimate) and an ERROR under --strict (a snapshot must build the pinned sources).

The per-repo CI half is tower-firmware's tools/protocol_pin_check.py (same rules; tower-firmware,
tower-cli and tower-hil CI all run it across checkouts of the three repos); that guards pushes;
this guards the local trees. Keep the rule set of the two in sync.
"""

import os
import re
import subprocess
import sys
from urllib.parse import parse_qs, urlsplit

if sys.version_info < (3, 11):
    sys.exit("lockstep: needs Python >= 3.11 (tomllib)")

import tomllib  # noqa: E402  (after the version guard on purpose)

CRATE = "tower-protocol"
CONSUMERS = ["firmware", "cli", "hil"]  # every tree that must pin the same tower-protocol
JOLT = "jolt"
JOLT_CONSUMERS = ["cli", "hil"]  # informational only
SKIP_DIRS = {"target", "node_modules", "__pycache__"}
_SHA = re.compile(r"^[0-9a-f]{40}$")
_DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


class Problems:
    def __init__(self, label: str = "ERROR") -> None:
        self.label = label
        self.errors: list[str] = []

    def error(self, msg: str) -> None:
        self.errors.append(msg)
        print(f"{self.label}: {msg}")


def norm_url(url: str) -> str:
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
    """Every Cargo.toml in the tree (skips build output, hidden dirs, nested git checkouts)."""
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


def crate_deps(table, crate: str = CRATE) -> list:
    out = []
    if not isinstance(table, dict):
        return out
    for key, spec in table.items():
        if key == crate or (isinstance(spec, dict) and spec.get("package") == crate):
            out.append((key, spec))
    return out


def iter_dep_tables(doc: dict):
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


def classify(spec, where: str, probs: Problems, crate: str = CRATE):
    if not isinstance(spec, dict):
        probs.error(f"{where}: {crate} = {spec!r} is a registry/version requirement; it must be "
                    "pinned by git tag")
        return None
    if "path" in spec:
        probs.error(f"{where}: {crate} has `path = {spec['path']!r}`: a local path must never be "
                    "committed (use an uncommitted `paths` override for co-development)")
        return None
    if "git" not in spec:
        probs.error(f"{where}: {crate} has no `git` source; it must be pinned by git tag")
        return None
    for bad in ("rev", "branch"):
        if bad in spec:
            probs.error(f"{where}: {crate} is pinned by `{bad} = {spec[bad]!r}`; it must be "
                        "pinned by `tag`")
            return None
    if "tag" not in spec:
        probs.error(f"{where}: {crate} has a git source but no `tag` (tracks the default branch)")
        return None
    return norm_url(spec["git"]), spec["tag"]


def config_overrides(cfg_dir: str, probs: Problems | None) -> list[tuple[str, str]]:
    """(config file relative to cfg_dir, description) of each tower-protocol override in
    <cfg_dir>/.cargo/config(.toml): `paths` entries that resolve to it, or a [patch] of it."""
    hits = []
    for name in ("config.toml", "config"):
        rel = os.path.join(".cargo", name)
        path = os.path.join(cfg_dir, rel)
        doc = load_toml(path, probs)
        if doc is None:
            continue
        for p in doc.get("paths") or []:
            target = p if os.path.isabs(p) else os.path.join(cfg_dir, p)
            sub = load_toml(os.path.join(target, "Cargo.toml"), None)
            if sub is None:
                hits.append((rel, f"{path}: `paths` override {p!r} is unresolvable (cannot rule "
                                  f"out that it shadows {CRATE})"))
            elif (sub.get("package") or {}).get("name") == CRATE:
                hits.append((rel, f"{path}: `paths` override {p!r} shadows {CRATE} with a local "
                                  "checkout"))
        for src, entries in (doc.get("patch") or {}).items():
            if crate_deps(entries):
                hits.append((rel, f"{path}: [patch.{src!r}] overrides {CRATE}"))
    return hits


def tree_pins(tree: str, probs: Problems, crate: str = CRATE) -> dict:
    """{where: (url, tag)} for every pin of `crate` in the tree (errors recorded on bad pins)."""
    root_doc = load_toml(os.path.join(tree, "Cargo.toml"), probs) or {}
    ws_deps = dict(crate_deps(((root_doc.get("workspace") or {}).get("dependencies")) or {}, crate))
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
            for key, spec in crate_deps(table, crate):
                where = f"{rel} {label} {key}"
                if isinstance(spec, dict) and spec.get("workspace") is True:
                    inherited = ws_deps.get(key)
                    if inherited is None:
                        probs.error(f"{where}: `workspace = true` but the workspace root defines "
                                    f"no [workspace.dependencies] {key}")
                        continue
                    spec = inherited
                got = classify(spec, where, probs, crate)
                if got:
                    pins[where] = got
        for src, entries in (doc.get("patch") or {}).items():
            for key, _ in crate_deps(entries, crate):
                probs.error(f"{rel} [patch.{src!r}] {key}: overrides {crate}, which must not be committed")
        for key in (doc.get("replace") or {}):
            if key.split(":")[0].split("@")[0] == crate:
                probs.error(f"{rel} [replace] {key!r}: overrides {crate}, which must not be committed")
    return pins


def locked(tree: str, probs: Problems, crate: str = CRATE):
    """(url, tag, sha) of the single git-tag-locked `crate` in <tree>/Cargo.lock, else None."""
    path = os.path.join(tree, "Cargo.lock")
    rel = os.path.relpath(path)
    doc = load_toml(path, probs)
    if doc is None:
        probs.error(f"{rel} missing: the resolved commit cannot be verified (every TOWER consumer "
                    "commits its lockfile)")
        return None
    pkgs = [p for p in doc.get("package", []) if p.get("name") == crate]
    if not pkgs:
        probs.error(f"{rel}: no {crate} package locked (run `cargo update -p {crate}`)")
        return None
    if len(pkgs) > 1:
        probs.error(f"{rel}: {len(pkgs)} {crate} packages locked "
                    f"({[p.get('version') for p in pkgs]}): two versions linked")
        return None
    src = pkgs[0].get("source", "")
    m = re.fullmatch(r"git\+([^?#]+)(?:\?([^#]*))?#([0-9a-fA-F]+)", src)
    if not m:
        probs.error(f"{rel}: {crate} source {src!r} is not a git source (path/registry override?); "
                    "the resolved commit cannot be verified")
        return None
    url, query, sha = norm_url(m.group(1)), parse_qs(m.group(2) or ""), m.group(3).lower()
    if "tag" not in query:
        probs.error(f"{rel}: {crate} locked by `{next(iter(query), 'default branch')}` ({src!r}), "
                    "not by tag")
        return None
    if not _SHA.match(sha):
        probs.error(f"{rel}: {crate} locked SHA {sha!r} is not a full 40-hex commit")
        return None
    print(f"  {sha}  {rel} (?tag={query['tag'][0]})")
    return url, query["tag"][0], sha


def git(args: list[str]):
    try:
        out = subprocess.run(["git", *args], capture_output=True, text=True, timeout=60, check=False)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return out.stdout.strip() if out.returncode == 0 else None


def remote_tag_commit(url: str, tag: str):
    out = git(["ls-remote", "--tags", url, f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}"])
    if not out:
        return None
    refs = dict(line.split("\t")[::-1] for line in out.splitlines())
    return (refs.get(f"refs/tags/{tag}^{{}}") or refs.get(f"refs/tags/{tag}") or "").lower() or None


def main(argv: list[str]) -> int:
    strict = "--strict" in argv
    remote = "--remote" in argv
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    if "--root" in argv:
        i = argv.index("--root")
        if i + 1 >= len(argv):
            sys.exit("lockstep: --root needs a directory")
        root = os.path.abspath(argv[i + 1])
    unknown = [a for a in argv if a.startswith("-") and a not in ("--strict", "--remote", "--root")]
    if unknown:
        sys.exit(f"lockstep: unknown option(s) {unknown} (see the module docstring)")
    os.chdir(root)
    probs = Problems()
    proto_git = os.path.exists(os.path.join("protocol", ".git"))

    # 1. Tag pins (every manifest of every consumer) + 2. the resolved lockfile SHAs.
    per = {}
    for name in CONSUMERS:
        if not os.path.isfile(os.path.join(name, "Cargo.toml")):
            probs.error(f"{name}/Cargo.toml missing; is the submodule initialized? "
                        "(git submodule update --init, or /bootstrap)")
            continue
        print(f"{name}:")
        pins = tree_pins(name, probs)
        for where, (_, tag) in sorted(pins.items()):
            print(f"  {tag:<10} {where}")
        # A COMMITTED override would make every clone build local sources: error. An untracked
        # (git-ignored) one is local co-development (tower-cli documents one): warning, or error
        # under --strict. Outside a git checkout we can't tell, so it counts as committed.
        in_repo = git(["-C", name, "rev-parse", "--is-inside-work-tree"]) is not None
        for rel, hit in config_overrides(name, probs):
            local = in_repo and git(["-C", name, "ls-files", "--error-unmatch", rel]) is None
            if local and not strict:
                print(f"WARNING: {hit} (untracked, local only); local builds use it instead of the "
                      "pinned tag, and /pin refuses while it exists")
            elif local:
                probs.error(f"{hit} (untracked); a snapshot must build the PINNED sources")
            else:
                probs.error(f"{hit} (committed / not provably local)")
        if not pins:
            probs.error(f"{name}: no `{CRATE} = {{ git = …, tag = … }}` pin found in any manifest; "
                        "refusing to report OK on nothing")
        lock = locked(name, probs)
        distinct = set(pins.values())
        if len(distinct) > 1:
            probs.error(f"{name}: {CRATE} pins DISAGREE inside the tree: {sorted(distinct)}")
        if lock and len(distinct) == 1:
            (url, tag), = distinct
            if lock[1] != tag:
                probs.error(f"{name}/Cargo.lock resolved ?tag={lock[1]} but the manifests pin {tag} "
                            f"(stale lockfile); run `cargo update -p {CRATE}` in {name}/")
            if lock[0] != url:
                probs.error(f"{name}/Cargo.lock source {lock[0]} != manifest git URL {url}")
        per[name] = (distinct, lock)

    tags = {t for d, _ in per.values() for _, t in d}
    urls = {u for d, _ in per.values() for u, _ in d}
    shas = {lk[2] for _, lk in per.values() if lk}
    if len(tags) > 1:
        probs.error(f"{CRATE} tag MISMATCH across consumers "
                    f"{ {n: sorted(t for _, t in d) for n, (d, _) in per.items()} }. postcard is not "
                    "self-describing: a mismatch silently mis-decodes the wire. Bump every consumer "
                    "in the same change-set (see the AGENTS.md runbook).")
    if len(urls) > 1:
        probs.error(f"{CRATE} git URL differs across consumers: {sorted(urls)}")
    if len(shas) > 1:
        probs.error(f"{CRATE} RESOLVED-SHA mismatch across lockfiles "
                    f"{ {n: lk[2][:12] for n, (_, lk) in per.items() if lk} }"
                    + (" although the tags agree (re-cut tag or stale cargo git cache)"
                       if len(tags) == 1 else "")
                    + f". Run `cargo update -p {CRATE}` in each consumer so all locks resolve to "
                    "the same commit.")

    # 3. The resolved SHA must be the commit the tag names in protocol/ (falls back to the
    #    remote when the local clone lacks the tag, e.g. a shallow CI submodule checkout).
    tag = sha = url = None
    if len(tags) == 1 and len(shas) == 1 and len(urls) == 1:
        (tag,), (sha,), (url,) = tags, shas, urls
        # Only ask protocol/ itself: in an uninitialized submodule dir, `git -C` would silently
        # walk up to the control-plane repo.
        local = proto_git and git(["-C", "protocol", "rev-parse", "--verify", "--quiet",
                                   f"refs/tags/{tag}^{{commit}}"])
        if local:
            print(f"protocol/ tag {tag} -> {local}")
            if local.lower() != sha:
                probs.error(f"protocol/ tag {tag} is {local[:12]} but every lockfile resolved "
                            f"{sha[:12]}: the tag was re-cut (or your local tag is stale: "
                            "`git -C protocol fetch --tags --force`). Investigate before pinning.")
        if remote or not local:
            rc = remote_tag_commit(url, tag)
            if rc is None:
                probs.error(f"tag {tag} not resolvable "
                            f"{'locally or ' if not local else ''}at {url}; cannot prove the locked "
                            "SHA is the tag's commit (git fetch --tags in protocol/, or check network)")
            else:
                print(f"{url} tag {tag} -> {rc}")
                if rc != sha:
                    probs.error(f"upstream tag {tag} is {rc[:12]} but every lockfile resolved "
                                f"{sha[:12]}: the tag was RE-CUT upstream after the consumers "
                                f"locked it; re-run `cargo update -p {CRATE}` everywhere.")

    # 4. Local co-development override at the control-plane root (git-ignored by design).
    for _, hit in config_overrides(".", None):
        if strict:
            probs.error(f"{hit}. A snapshot must build the PINNED sources; remove the override")
        else:
            print(f"WARNING: {hit}. Local builds use it instead of the pinned tag (fine for "
                  "co-development; /pin refuses while it exists)")

    # 5. Informational: where is the protocol/ checkout relative to the pinned tag's COMMIT?
    head = proto_git and git(["-C", "protocol", "rev-parse", "HEAD"])
    pv = ((load_toml("protocol/Cargo.toml", None) or {}).get("package") or {}).get("version")
    if head and sha:
        if head == sha:
            rel = f"exactly the pinned {tag} commit"
        elif git(["-C", "protocol", "merge-base", "--is-ancestor", sha, "HEAD"]) is not None:
            n = git(["-C", "protocol", "rev-list", "--count", f"{sha}..HEAD"])
            rel = f"{n} commit(s) ahead of the pinned {tag} commit (unreleased protocol work?)"
        else:
            rel = f"NOT a descendant of the pinned {tag} commit (other branch, or older)"
        print(f"protocol/ checkout: {head[:12]} (Cargo.toml version {pv}), {rel}")

    # 6. Informational: the jolt pins (cli + hil). A mismatch is a compile error in the consumer,
    #    not a silent-interop hazard, so it never fails this check.
    jolt_probs = Problems("  note")  # recorded but never fails the check
    jolt_tags = {}
    for name in JOLT_CONSUMERS:
        if os.path.isdir(name):
            for where, (_, t) in sorted(tree_pins(name, jolt_probs, JOLT).items()):
                jolt_tags[where] = t
                print(f"  jolt {t}  {where}")
    if len(set(jolt_tags.values())) > 1:
        print("note: jolt tags differ between consumers (compile-error class, not silent).")

    if probs.errors:
        print(f"lockstep BROKEN: {len(probs.errors)} problem(s) above; fix via the coordinated "
              "bump runbook in AGENTS.md (don't hand-edit a single consumer).")
        return 1
    print(f"lockstep OK: {', '.join(CONSUMERS)} pin {CRATE} {tag} @ {sha}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
