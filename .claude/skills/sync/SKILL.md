---
name: sync
description: Fast-forward each TOWER submodule (protocol, jolt, cli, firmware, hil) to its upstream main, then verify the tower-protocol lockstep across firmware, cli and hil. Moves the child working trees (never commits, never forces); `--check` makes it read-only. Run explicitly as /sync before /build and /pin.
argument-hint: "[firmware|cli|protocol|jolt|hil] [--check]"
disable-model-invocation: true
allowed-tools:
  - Bash(git submodule status)
  - Bash(git -C protocol fetch origin --tags --prune)
  - Bash(git -C jolt fetch origin --tags --prune)
  - Bash(git -C cli fetch origin --tags --prune)
  - Bash(git -C firmware fetch origin --tags --prune)
  - Bash(git -C hil fetch origin --tags --prune)
  - Bash(git -C protocol log *)
  - Bash(git -C jolt log *)
  - Bash(git -C cli log *)
  - Bash(git -C firmware log *)
  - Bash(git -C hil log *)
  - Bash(git -C protocol status --porcelain)
  - Bash(git -C jolt status --porcelain)
  - Bash(git -C cli status --porcelain)
  - Bash(git -C firmware status --porcelain)
  - Bash(git -C hil status --porcelain)
  - Bash(git -C protocol switch main)
  - Bash(git -C jolt switch main)
  - Bash(git -C cli switch main)
  - Bash(git -C firmware switch main)
  - Bash(git -C hil switch main)
  - Bash(git -C protocol merge --ff-only origin/main)
  - Bash(git -C jolt merge --ff-only origin/main)
  - Bash(git -C cli merge --ff-only origin/main)
  - Bash(git -C firmware merge --ff-only origin/main)
  - Bash(git -C hil merge --ff-only origin/main)
  - Bash(git -C protocol tag --sort=-v:refname)
  - Bash(git -C jolt tag --sort=-v:refname)
  - Bash(git -C protocol describe --tags --abbrev=0)
  - Bash(python3 tools/check_lockstep.py)
  - Bash(head *)
  - Read
  - Grep
---

# Sync the TOWER ecosystem to upstream

Advance the submodules to the latest upstream `main` and then (the part that actually
matters) check that **`tower-protocol` is pinned to the same tag, resolved to the same commit,
in `firmware`, `cli`, and `hil`**. The wire format is postcard (non self-describing): if the
consumers reference different protocol versions, frames are *silently* mis-decoded rather than
rejected. This skill exists to catch that.

Argument `$ARGUMENTS`:
- a repo name limits the pull to that submodule (the lockstep check always runs);
- `--check` makes this read-only: fetch and report drift, change no working tree.

## 1. Pull each in-scope submodule to upstream

For each of `protocol`, `jolt`, `cli`, `firmware`, `hil` (this order: protocol and jolt are the
dependencies):

```bash
git -C <repo> fetch origin --tags --prune
git -C <repo> log --oneline --decorate HEAD..origin/main   # what's incoming
git -C <repo> status --porcelain                           # must be empty to move
```

If incoming commits exist and `--check` was NOT passed, fast-forward the child to `main` (these
repos develop straight on `main`; after `/bootstrap` a child sits detached at the pinned SHA, and
`switch main` re-attaches it):

```bash
git -C <repo> switch main
git -C <repo> merge --ff-only origin/main
```

If the working tree is dirty, `switch` fails, or the fast-forward is refused (local commits in
the submodule, or a diverged local `main`), STOP for that repo and report it; do not force,
stash, rebase, reset, or discard. Report the new short SHA and one-line subject for each repo
that moved.

## 2. Verify the tower-protocol lockstep (always, even with --check)

This is the gate; see the `/lockstep` skill for exactly what it checks (every manifest of every
consumer, the resolved lockfile commits, and the commit the tag names in `protocol/`):

```bash
python3 tools/check_lockstep.py
```

Find the latest tag actually available in the protocol submodule:

```bash
git -C protocol tag --sort=-v:refname | head -5
git -C protocol describe --tags --abbrev=0
```

Then evaluate and report:
- **script verdict**: a non-zero exit is a lockstep break. Flag it loudly (quote the `ERROR:`
  lines) and explain that this causes silent decode failures.
- **pinned tag vs latest protocol release**: if the consumers pin an older tag than the latest
  protocol release, note that an upgrade is available (applied to *all* consumers together, then
  `cargo update -p tower-protocol` in each, per the `AGENTS.md` runbook).
- the script's informational line on where the `protocol/` checkout sits relative to the pinned
  tag's commit.

Do not edit any `Cargo.toml`/`Cargo.lock` here; fixing a lockstep break is a deliberate change
the user drives (the protocol repo's own `AGENTS.md` has the release runbook). Surface the exact
lines that would need to change and ask before touching them.

## 2b. Check the jolt pin (informational, not a gate)

`cli` and `hil` also pin `jolt` by tag (the script prints both). A `jolt` mismatch is a
**compile error** in the consumer, not a silent failure, but if the `jolt` submodule advanced
past the pinned tags, note that an upgrade is available:

```bash
git -C jolt tag --sort=-v:refname | head -3
```

## 3. Report

Print a table: repo | old SHA → new SHA | incoming commits | protocol tag pinned .
End with the lockstep verdict (✅ aligned / ⚠️ drift) and the recommended next step: if everything
pulled and lockstep holds, suggest `/build` and then `/pin` to freeze the new known-good set.
Do **not** commit the submodule pointer bumps or push anything; `/pin` records the snapshot.
