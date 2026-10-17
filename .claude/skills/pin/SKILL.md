---
name: pin
description: Freeze the commits the five TOWER submodules currently sit on as a known-good snapshot; safety-check them (clean, pushed to origin/main, protocol lockstep, no co-dev override), stage the gitlink bumps and commit them in the control plane. Never pushes. Run explicitly as /pin after /sync + /build.
argument-hint: "[-m \"message\"] [--tag vX.Y.Z]"
disable-model-invocation: true
allowed-tools:
  - Bash(git submodule status)
  - Bash(git diff --submodule=log -- firmware cli protocol jolt hil)
  - Bash(git -C protocol log -1 --oneline)
  - Bash(git -C firmware log -1 --oneline)
  - Bash(git -C cli log -1 --oneline)
  - Bash(git -C jolt log -1 --oneline)
  - Bash(git -C hil log -1 --oneline)
  - Bash(git -C protocol describe --tags --always)
  - Bash(git -C firmware describe --tags --always)
  - Bash(git -C cli describe --tags --always)
  - Bash(git -C jolt describe --tags --always)
  - Bash(git -C hil describe --tags --always)
  - Bash(git -C protocol status --porcelain)
  - Bash(git -C firmware status --porcelain)
  - Bash(git -C cli status --porcelain)
  - Bash(git -C jolt status --porcelain)
  - Bash(git -C hil status --porcelain)
  - Bash(git -C protocol fetch origin)
  - Bash(git -C firmware fetch origin)
  - Bash(git -C cli fetch origin)
  - Bash(git -C jolt fetch origin)
  - Bash(git -C hil fetch origin)
  - Bash(git -C protocol merge-base --is-ancestor HEAD origin/main)
  - Bash(git -C firmware merge-base --is-ancestor HEAD origin/main)
  - Bash(git -C cli merge-base --is-ancestor HEAD origin/main)
  - Bash(git -C jolt merge-base --is-ancestor HEAD origin/main)
  - Bash(git -C hil merge-base --is-ancestor HEAD origin/main)
  - Bash(python3 tools/check_lockstep.py --strict)
  - Bash(test -f .cargo/config.toml)
  - Bash(test -f .cargo/config)
  - Bash(git add firmware cli protocol jolt hil .gitmodules)
  - Bash(git status --short)
  - Bash(git commit -F *)
  - Bash(git commit -m *)
  - Bash(git tag -a *)
  - Bash(git log -1 --stat)
  - Read
  - Grep
---

# Pin the TOWER ecosystem to a known-good snapshot

Record the commits the five submodules currently sit on into THIS control-plane repo as a
single coherent snapshot. This is the inverse of `/sync`: sync advances the working trees, pin
freezes them into a commit you can return to later.

Optional `$ARGUMENTS`: `-m "message"` overrides the commit message; `--tag vX.Y.Z` also tags the
control-plane commit.

## 1. Show the proposed snapshot

```bash
git submodule status
git diff --submodule=log -- firmware cli protocol jolt hil
```

For each submodule capture the new short SHA, its one-line subject, and the tag it is on:

```bash
git -C <repo> log -1 --oneline
git -C <repo> describe --tags --always
```

The `tower-protocol` tag that firmware, cli and hil share, and the `jolt` tag cli and hil pin
(for the message), come from the lockstep check's output in step 2.

## 2. Safety checks: REFUSE to pin unsafe state

A pin is only useful if anyone who clones the control plane can resolve these exact commits and
build them. Before staging anything, verify for EACH submodule:

- **No uncommitted changes inside the child.** `git -C <repo> status --porcelain` must be empty.
  If not, the child has work that belongs in a child commit first: stop and tell the user; do
  not pin over a dirty tree.
- **The checked-out commit is on the child's upstream `main`.** Refresh the remote-tracking ref
  first (a stale one can hide a commit that was never pushed, or one that was force-pushed away),
  then test ancestry:
  ```bash
  git -C <repo> fetch origin
  git -C <repo> merge-base --is-ancestor HEAD origin/main   # exit 0 = pushed
  ```
  A non-zero exit means the commit is not on upstream `main`: pinning would record a SHA a fresh
  clone may not fetch. Stop and tell the user to push the child first (commit/push in children is
  user-driven).
- **The protocol lockstep holds, strictly.** A "known-good" snapshot with a mismatched
  `tower-protocol` silently mis-decodes the wire. Run:
  ```bash
  python3 tools/check_lockstep.py --strict
  ```
  It checks every manifest pin across the LOCAL trees (exactly what is being pinned), that every
  `Cargo.lock` resolves the same commit, that the commit is the one the tag names in `protocol/`,
  and (`--strict`) that no co-development `paths`/`[patch]` override (git-ignored root or
  child `.cargo/config.toml`) shadows the pinned sources. Refuse to pin on a non-zero exit.
  (`/sync` runs the check too, but `/pin` must not assume `/sync` ran.)
- **No local override at the root**, belt and braces: `test -f .cargo/config.toml` (and
  `.cargo/config`) must fail. If one exists, builds validated LOCAL (possibly unpushed) sources,
  not the pinned tags; stop and tell the user to remove the co-dev override first.

If any check fails, report exactly which repo and why, and do not commit.

## 3. Stage and commit the gitlink bumps

```bash
git add firmware cli protocol jolt hil .gitmodules
git status --short
```

If nothing is staged, report "already pinned, no submodule changes" and stop. Otherwise commit
(write the message to a temp file and use `git commit -F <file>`, or `-m`). Default message when
`-m` is not given:

```
pin: firmware@<sha> · cli@<sha> · protocol@<sha> · jolt@<sha> · hil@<sha>

firmware  <sha>  <subject>
cli       <sha>  <subject>
protocol  <sha>  <subject>  (release <protocol-tag>)
jolt      <sha>  <subject>  (release <jolt-tag>)
hil       <sha>  <subject>
tower-protocol pinned by firmware & cli & hil: <tag> @ <resolved sha>   ·   jolt pinned by cli & hil: <tag>
```

Then, if `--tag` was supplied:

```bash
git tag -a <tag> -m "TOWER snapshot <tag>"
```

## 4. Report

Show `git log -1 --stat` for the new pin commit and list the frozen SHAs. Do **not** push unless
the user explicitly asks; then it is `git push` (and `git push origin <tag>` if a tag was
created); the control plane's CI re-verifies the pinned snapshot (recursive checkout + strict
lockstep) on push.
