---
description: Verify the tower-protocol lockstep across the local submodule trees (firmware ×2, cli, hil) — tag pins + resolved lockfile SHAs
argument-hint: ""
allowed-tools: Bash(python3:*), Bash(git -C:*), Bash(grep:*), Read
---

# /lockstep — the golden-rule check

`tower-protocol` is postcard on the wire — **not self-describing** — so consumers pinning
different tags silently mis-decode instead of erroring. This command checks the invariant
across the **local working trees** (exactly what `/pin` would freeze), without pulling or
changing anything.

## 1. Run the deterministic check

```bash
python3 tools/check_lockstep.py
```

It verifies:
- the **tag pins** agree across `firmware/Cargo.toml`, `firmware/crates/tower-kv/Cargo.toml`,
  `cli/Cargo.toml`, and `hil/Cargo.toml`;
- the **resolved SHAs** agree across `firmware/Cargo.lock`, `cli/Cargo.lock`, and
  `hil/Cargo.lock` — tags matching is not sufficient; a re-cut tag (same name, new commit)
  passes every tag check while the repos build different code;
- informationally: the `protocol/` working-tree version vs the pin, and the `jolt` tags
  (cli + hil; a jolt mismatch is a compile error, not an interop hazard).

## 2. Add the context the script can't see

```bash
git -C protocol tag --sort=-v:refname | head -3
git -C protocol describe --tags --always
```

- If both ends pin an older tag than the latest protocol release, note the available upgrade
  (it must be applied to **all** consumers together — the CLAUDE.md runbook).
- If the `protocol` submodule's checked-out commit is not the pinned tag's commit, say so.

## 3. Report

One short table: manifest | pinned tag , then lock | resolved SHA , then the verdict
(✅ lockstep holds / ⚠️ BREAK — with the exact manifests that disagree). On a break, do **not**
edit any manifest yourself — fixing it is the coordinated bump runbook in `CLAUDE.md`, driven
by the user. The firmware-repo CI runs the push-time half of this gate
(`firmware/tools/protocol_pin_check.py`, fetching the tower-cli + tower-hil pins from GitHub).
