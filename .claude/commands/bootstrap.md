---
description: First-run setup of the TOWER control plane — clone/init submodules, verify toolchains, report ecosystem status
argument-hint: "[--build] [firmware|cli|protocol|jolt]"
allowed-tools: Bash(git submodule:*), Bash(git -C:*), Bash(git status:*), Bash(git log:*), Bash(rustup:*), Bash(cargo:*), Bash(probe-rs:*), Bash(which:*), Bash(ls:*), Bash(cat:*), Read
---

# Bootstrap the TOWER ecosystem

You are setting up a freshly cloned **TOWER control plane** so the user can work
on the whole ecosystem from this one session. The child repos
(`firmware`, `cli`, `protocol`, `jolt`) are Git submodules at the repo root.

Optional argument `$ARGUMENTS`:
- a repo name (`firmware` | `cli` | `protocol` | `jolt`) limits checks to that one repo;
- `--build` additionally compiles each in-scope repo.

Work through these steps and report a concise status table at the end. Stop and
surface any error rather than pushing past it.

## 1. Materialize the submodules
```bash
git submodule update --init --recursive --progress
```
Then confirm each is checked out at the SHA recorded by the control plane:
```bash
git submodule status
```
A leading `-` means "not initialized" (rerun the update), `+` means the working
tree is ahead of the pinned SHA (note it; `/pin` records it, `/sync` advances it),
and a bare SHA means it matches the pin. Report which state each repo is in.

## 2. Verify toolchains (read, don't assume)
For each in-scope repo, read its toolchain requirements rather than hardcoding:
- `cat <repo>/rust-toolchain.toml` (channel + targets + components) if present.
- Ensure the channel is installed: `rustup toolchain list`.
- Ensure every `targets` entry is installed: `rustup target list --installed`,
  and `rustup target add <triple>` for any that are missing.
- For **firmware** specifically, it is an embedded target — also check the flashing
  toolchain is present: `which probe-rs` (or read `firmware/Embed.toml` /
  `.cargo/config.toml` to learn what the repo actually uses) and report if absent
  with the install hint `cargo install probe-rs-tools`.
- For **cli** and **jolt** on Linux: both link `serialport`, which needs `libudev`
  dev headers — if a build fails there, the hint is `libudev-dev` + `pkg-config`.

## 3. Sanity-check the workspace
- `git -C protocol log -1 --oneline`, same for `cli`, `firmware`, and `jolt`, to show
  the exact commit each repo sits on.
- Read each repo's `CLAUDE.md`/`AGENTS.md` if present so you carry its house rules
  into this session; mention any that exist.

## 4. Optional build (`--build`)
Only if `--build` was passed. Build in dependency order — **protocol → jolt → cli →
firmware** (`protocol` is the shared crate firmware+cli depend on; `cli` also links
`jolt`):
- protocol: `cargo build --manifest-path protocol/Cargo.toml`
- jolt: `cargo build --manifest-path jolt/Cargo.toml`
- cli: `cargo build --manifest-path cli/Cargo.toml`
- firmware: build per its own README/`.cargo/config.toml` (embedded target; do
  **not** assume a plain `cargo build` — it may need `--target`, a runner, or hardware).

## 5. Report
Print a table: repo | pinned SHA (short) | last-commit subject | toolchain OK? | built? .
Then state in one line whether the ecosystem is ready and what the user should do
next (`/sync` to pull upstream, `/pin` to freeze a known-good set).
