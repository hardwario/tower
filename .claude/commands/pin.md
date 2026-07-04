---
description: Freeze the current submodule commits as a known-good snapshot — stage the gitlink bumps and commit them in the control plane
argument-hint: "[-m \"message\"] [--tag vX.Y.Z]"
allowed-tools: Bash(git -C:*), Bash(git submodule:*), Bash(git add:*), Bash(git status:*), Bash(git diff:*), Bash(git commit:*), Bash(git log:*), Bash(git tag:*), Bash(git branch:*), Bash(grep:*), Read
---

# Pin the TOWER ecosystem to a known-good snapshot

Record the commits the four submodules currently sit on into THIS control-plane
repo as a single coherent snapshot. This is the inverse of `/sync`: sync advances
the working trees, pin freezes them into a commit you can return to later.

Optional `$ARGUMENTS`: `-m "message"` overrides the commit message; `--tag vX.Y.Z`
also tags the control-plane commit.

## 1. Show the proposed snapshot
```bash
git submodule status
git diff --submodule=log -- firmware cli protocol jolt
```
For each submodule capture the new short SHA, its one-line subject, and the
branch/tag it is on:
```bash
git -C <repo> log -1 --oneline
git -C <repo> describe --tags --always
```
Also capture the dependency tags pinned in `Cargo.toml` (for the message): the
`tower-protocol` tag firmware and cli share, and the `jolt` tag cli pins:
```bash
grep -h "tower-protocol" firmware/Cargo.toml cli/Cargo.toml
grep -h "jolt" cli/Cargo.toml
```

## 2. Safety checks — REFUSE to pin unsafe state
A pin is only useful if anyone who clones the control plane can resolve these
exact commits. Before staging anything, verify for EACH submodule:
- **No uncommitted changes inside the child.** `git -C <repo> status --porcelain`
  must be empty. If not, the child has work that belongs in a child commit first —
  stop and tell the user; do not pin over a dirty tree.
- **The checked-out commit is pushed to its remote.** Check
  `git -C <repo> branch -r --contains HEAD`. If the commit is on no remote branch,
  pinning would record a SHA that no fresh clone can fetch. Stop and tell the user
  to push the child first (commit/push in children is user-driven).
- **The protocol lockstep holds.** A "known-good" snapshot with a mismatched
  `tower-protocol` pin silently mis-decodes the wire. Run
  `python3 firmware/tools/protocol_pin_check.py --cli-url https://raw.githubusercontent.com/hardwario/tower-cli/main/Cargo.toml`
  — it now also cross-checks the RESOLVED `Cargo.lock` SHAs, so a re-cut tag (same string,
  different commit) is caught too. Refuse to pin on any mismatch. (`/sync` runs this, but `/pin`
  must not assume `/sync` ran.)
- **No local `paths` override is shadowing the pinned sources.** If a git-ignored root
  `.cargo/config.toml` exists, builds validated LOCAL (possibly unpushed) sources, not the pinned
  tags — a fresh clone would build different code. Refuse to pin while one is present
  (`test -f .cargo/config.toml` → stop and tell the user to remove the co-dev override first).

If any check fails, report exactly which repo and why, and do not commit.

## 3. Stage and commit the gitlink bumps
```bash
git add firmware cli protocol jolt .gitmodules
git status --short
```
If nothing is staged, report "already pinned — no submodule changes" and stop.
Otherwise commit. Default message when `-m` is not given:
```
pin: firmware@<sha> · cli@<sha> · protocol@<sha> · jolt@<sha>

firmware  <sha>  <subject>
cli       <sha>  <subject>
protocol  <sha>  <subject>  (release <protocol-tag>)
jolt      <sha>  <subject>  (release <jolt-tag>)
tower-protocol pinned by firmware & cli: <tag>   ·   jolt pinned by cli: <tag>
```
Run the commit, then if `--tag` was supplied:
```bash
git tag -a <tag> -m "TOWER snapshot <tag>"
```

## 4. Report
Show `git log -1 --stat` for the new pin commit and list the frozen SHAs. Do **not**
push unless the user explicitly asks — then it is `git push` (and `git push --tags`
if a tag was created).
