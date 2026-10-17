---
name: lockstep
description: Verify the tower-protocol lockstep across the local firmware, cli and hil working trees (every manifest pin, every resolved Cargo.lock commit, and the commit the tag names in protocol/); read-only, changes nothing. Use before /pin, after any tower-protocol bump, or when the console decodes garbage / reports BadVersion.
argument-hint: "[--remote] [--strict]"
allowed-tools:
  - Bash(python3 tools/check_lockstep.py)
  - Bash(python3 tools/check_lockstep.py --remote)
  - Bash(python3 tools/check_lockstep.py --strict)
  - Bash(python3 tools/check_lockstep.py --remote --strict)
  - Bash(git -C protocol tag --sort=-v:refname)
  - Bash(git -C protocol describe --tags --always)
  - Bash(git -C protocol log *)
  - Bash(head *)
  - Read
  - Grep
---

# /lockstep: the golden-rule check

`tower-protocol` is postcard on the wire (**not self-describing**), so consumers pinning
different tags (or different commits under one tag) silently mis-decode instead of erroring.
This skill checks the invariant across the **local working trees** (exactly what `/pin` would
freeze), without pulling or changing anything.

Optional `$ARGUMENTS` are passed through to the script: `--remote` also resolves the tag with
`git ls-remote` (catches a tag re-cut upstream after the local clone fetched it); `--strict`
also fails on a local co-development `paths` override (what `/pin` uses).

## 1. Run the deterministic check

```bash
python3 tools/check_lockstep.py $ARGUMENTS
```

It fails closed (exit 1, one `ERROR:` line per problem) unless all of this holds:
- **every** `Cargo.toml` in `firmware/`, `cli/`, `hil/` (a filesystem walk, so new workspace crates
  count; parsed as TOML, so a commented-out line can't shadow the real pin; `[dependencies]`,
  `[dev-dependencies]`, `[build-dependencies]`, `[target.*]`, `[workspace.dependencies]`,
  renamed and `workspace = true` deps all count) pins `tower-protocol` as
  `{ git = <one url>, tag = "<one tag>" }`: no `rev`/`branch`/`path`, no `[patch]`/`[replace]`;
- no **committed** `.cargo/config.toml` in a consumer redirects it (an untracked, git-ignored
  co-dev override, like the one tower-cli documents, is a WARNING, an error under `--strict`);
- each consumer's `Cargo.lock` holds exactly one `tower-protocol`, sourced `?tag=<the same
  tag>#<sha>`, and the SHA is identical across all three; tag strings agreeing is not enough
  (a re-cut tag, same name/new commit, happened 2026-07-02);
- that SHA is the commit the tag names in `protocol/` (`git -C protocol rev-parse <tag>^{commit}`,
  falling back to `git ls-remote` when the local clone lacks the tag).

Informational lines (never fail): where `protocol/` sits relative to the pinned tag's commit
(exactly at it / N commits ahead / diverged), and the `jolt` pins of cli + hil (a jolt mismatch is
a compile error, not an interop hazard).

## 2. Add the context the script can't see

```bash
git -C protocol tag --sort=-v:refname | head -3
git -C protocol describe --tags --always
```

- If the consumers pin an older tag than the latest protocol release, note the available upgrade
  (it must be applied to **all** consumers together, per the `AGENTS.md` runbook).

## 3. Report

One short table: manifest | pinned tag , then lock | resolved SHA , then the verdict
(✅ lockstep holds / ⚠️ BREAK, quoting each `ERROR:` line). On a break, do **not** edit any
manifest or lockfile yourself; fixing it is the coordinated bump runbook in `AGENTS.md`, driven
by the user. The push-time half of this gate runs in CI: tower-firmware's
`tools/protocol_pin_check.py` (same rules), run by the firmware, cli and hil CI over checkouts of
all three repos; the control plane's own CI runs `tools/check_lockstep.py --strict --remote` on
the pinned submodules.
