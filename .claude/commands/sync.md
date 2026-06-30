---
description: Pull each submodule up to its upstream default branch, then verify tower-protocol lockstep across firmware & cli
argument-hint: "[firmware|cli|protocol] [--check]"
allowed-tools: Bash(git submodule:*), Bash(git -C:*), Bash(git fetch:*), Bash(git log:*), Bash(git status:*), Bash(git switch:*), Bash(git merge:*), Bash(git describe:*), Bash(git tag:*), Bash(grep:*), Bash(cat:*), Read
---

# Sync the TOWER ecosystem to upstream

Advance the submodules to the latest upstream of their default branch and then —
the part that actually matters — check that **`tower-protocol` is pinned to the
same tag in `firmware` and `cli`**. The wire format is postcard (non
self-describing): if the two ends reference different protocol tags, frames are
*silently* mis-decoded rather than rejected. This command exists to catch that.

Argument `$ARGUMENTS`:
- a repo name limits the pull to that submodule (the lockstep check always runs);
- `--check` makes this read-only: report drift, change nothing.

## 1. Pull each in-scope submodule to upstream
For each of `protocol`, `jolt`, `cli`, `firmware` (this order — protocol and jolt are
the dependencies):
```bash
git -C <repo> fetch origin --tags --prune
git -C <repo> log --oneline --decorate HEAD..origin/HEAD   # what's incoming
```
If incoming commits exist and `--check` was NOT passed, fast-forward the child to
its default branch (these repos develop straight on `main`):
```bash
git -C <repo> switch main
git -C <repo> merge --ff-only origin/main
```
If the fast-forward is refused (local commits in the submodule, or detached work),
STOP for that repo and report it — do not force, rebase, or discard. Report the
new short SHA and one-line subject for each repo that moved.

## 2. Verify tower-protocol lockstep (always, even with --check)
This is the gate. Gather the pinned protocol tag everywhere it is declared:
```bash
grep -rn "tower-protocol" firmware/Cargo.toml firmware/crates/*/Cargo.toml firmware/tools/*/Cargo.toml cli/Cargo.toml 2>/dev/null
```
Find the latest tag actually available in the protocol submodule:
```bash
git -C protocol tag --sort=-v:refname | head -5
git -C protocol describe --tags --abbrev=0
```
Then evaluate and report:
- **firmware tag vs cli tag** — if they differ, this is a lockstep break. Flag it
  loudly and explain that this causes silent decode failures.
- **pinned tag vs latest protocol release** — if both ends pin an older tag than
  the latest protocol release, note that an upgrade is available (and that it must
  be applied to *both* ends together, then `cargo update -p tower-protocol` in each).
- whether the `protocol` submodule's checked-out commit actually corresponds to
  the tag the others reference.

Do not edit any `Cargo.toml` here — fixing a lockstep break is a deliberate change
the user drives (and the protocol repo's own `CLAUDE.md` has the bump runbook).
Surface the exact lines that would need to change and ask before touching them.

## 2b. Check the jolt pin (informational, not a gate)
`cli` also pins `jolt` by tag. There is only one consumer, so this is **not** a
lockstep gate — but if the `jolt` submodule advanced past the pinned tag, note that an
upgrade is available. Unlike protocol, a `jolt` mismatch is a **compile error** in
`cli`, not a silent failure, so it is safe to just flag:
```bash
grep -h "jolt" cli/Cargo.toml
git -C jolt tag --sort=-v:refname | head -3
```

## 3. Report
Print a table: repo | old SHA → new SHA | incoming commits | protocol tag pinned .
End with the lockstep verdict (✅ aligned / ⚠️ drift) and the recommended next step:
if everything pulled and lockstep holds, suggest building (`/bootstrap --build`) and
then `/pin` to freeze the new known-good set. Do **not** commit the submodule pointer
bumps or push anything — `/pin` records the snapshot.
