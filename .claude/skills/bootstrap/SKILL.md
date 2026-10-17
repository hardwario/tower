---
name: bootstrap
description: First-run setup of the TOWER control plane; initialize only the submodules that are missing (never rewinding ones already checked out), verify the Rust toolchains and the firmware/flashing tools each repo really needs, and report ecosystem status. Run explicitly as /bootstrap on a fresh clone.
argument-hint: "[firmware|cli|protocol|jolt|hil]"
disable-model-invocation: true
allowed-tools:
  - Bash(git submodule status)
  - Bash(git submodule update --init -- *)
  - Bash(git -C protocol log -1 --oneline)
  - Bash(git -C firmware log -1 --oneline)
  - Bash(git -C cli log -1 --oneline)
  - Bash(git -C jolt log -1 --oneline)
  - Bash(git -C hil log -1 --oneline)
  - Bash(rustup toolchain list)
  - Bash(rustup target list --installed *)
  - Bash(rustup component list --installed *)
  - Bash(rustup toolchain install *)
  - Bash(rustup target add *)
  - Bash(rustup component add *)
  - Bash(cargo --version)
  - Bash(cargo install --list)
  - Bash(just --version)
  - Bash(tower --version)
  - Bash(probe-rs --version)
  - Bash(python3 --version)
  - Bash(which *)
  - Bash(ls *)
  - Read
  - Grep
---

# Bootstrap the TOWER ecosystem

You are setting up a freshly cloned **TOWER control plane** so the user can work on the whole
ecosystem from this one session. The child repos (`firmware`, `cli`, `protocol`, `jolt`, `hil`)
are Git submodules at the repo root.

Optional argument `$ARGUMENTS`: a repo name (`firmware` | `cli` | `protocol` | `jolt` | `hil`)
limits the checks to that one repo. Bootstrap verifies setup; to compile the code, use `/build`
afterward.

Work through these steps and report a concise status table at the end. Stop and surface any
error rather than pushing past it. Bootstrap never installs cargo tools itself; it reports what
is missing with the exact install command and lets the user decide.

## 1. Inspect the submodules FIRST, then initialize only what is missing

```bash
git submodule status
```

Read the prefix of each line before touching anything:
- `-`: **not initialized**. Initialize just those paths (no `--recursive` needed; the
  children have no nested submodules):
  ```bash
  git submodule update --init -- <path> [<path> …]
  ```
- `+`: checked out at a commit **different** from the one the control plane records (typically
  ahead after a `/sync`, or someone's in-progress work). Leave it alone and report it: `/pin`
  records it, `/sync` advances it. **Never** run a blanket `git submodule update` over it; that
  would rewind the child to the pinned SHA as a detached HEAD.
- ` ` (blank): matches the pin. Nothing to do.
- `U`: merge conflict in the gitlink. Stop and report.

Run `git submodule status` again after any init and report each repo's state.

## 2. Verify toolchains (read, don't assume)

For each in-scope repo, read `<repo>/rust-toolchain.toml` (channel + components + targets), the
toolchain pin every repo shares (CI installs exactly that). Then:
- `rustup toolchain list`: the pinned channel must be installed; if not:
  `rustup toolchain install <channel> --profile minimal --component <components…> --target <targets…>`
  (taken from the file: explicit, rather than relying on rustup's deprecated auto-install).
- `rustup target list --installed --toolchain <channel>` and
  `rustup component list --installed --toolchain <channel>`: every `targets`/`components` entry
  must be present; `rustup target add --toolchain <channel> <triple>` /
  `rustup component add --toolchain <channel> <name>` for any that are missing.
- **cli**, **jolt**, **hil** on Linux: all link `serialport`, which needs the `libudev` dev
  headers; if a build fails there, the hint is `libudev-dev` + `pkg-config`.

### firmware: what it actually needs (see `firmware/.cargo/config.toml` + `firmware/justfile`)
- `just`: the task runner (`just --version`; CI uses 1.59.0). Install: `cargo install --locked just`
  or a package manager.
- `flip-link`: the **linker** set in `firmware/.cargo/config.toml` (stack-overflow protection);
  without it nothing links. `which flip-link`; install: `cargo install --locked flip-link`.
- `cargo-binutils`: `just build`/`just size` run `cargo objcopy`/`cargo size` (check
  `cargo install --list`); install: `cargo install --locked cargo-binutils`. The `llvm-tools`
  component they drive comes from `firmware/rust-toolchain.toml`.
- the `tower` CLI on `PATH`: `just flash`/`just run` program the board over the UART bootloader
  and open the console with it (`tower --version`). Build it from this checkout:
  `cargo install --locked --path cli`. It must be rebuilt after every tower-protocol bump (an
  old binary reports BadVersion against new firmware).
- `python3` ≥ 3.11, for `/lockstep` (`tools/check_lockstep.py`, tomllib) and the firmware RAM-budget
  guard.
- Optional: `probe-rs` (`cargo install --locked probe-rs-tools`), only for SWD flashing
  (`cargo run`, the `runner` in `.cargo/config.toml`) and hil's power group; UART flashing via
  `tower` does not need it.

### hil
- `just`, the `tower` CLI and `python3` (as above); the power group also needs `probe-rs` and a
  `hil/.venv` with the PPK2 deps (`hil/docs/bench.md`). Report, don't install.

## 3. Sanity-check the workspace

- `git -C <repo> log -1 --oneline` for `protocol`, `cli`, `firmware`, `jolt`, and `hil`, to show
  the exact commit each repo sits on.
- Read each repo's `AGENTS.md` (the per-repo `CLAUDE.md` files just import it; `jolt`'s notes live
  in its `README.md`) so you carry its house rules into this session; mention any that exist.

## 4. Report

Print a table: repo | submodule state (`-`→initialized / `+` differs from pin / matches) | SHA
(short) | last-commit subject | toolchain OK? | missing tools . Then state in one line whether
the ecosystem is ready and what the user should do next (`/build` to compile, `/sync` to pull
upstream, `/lockstep` to check the protocol pins, `/pin` to freeze a known-good set).
