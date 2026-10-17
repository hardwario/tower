---
name: bootstrap
description: First-run check of a TOWER monorepo clone; verify rustup and the pinned Rust toolchain (components + thumbv6m target), just, flip-link, cargo-binutils, python3 >= 3.11 and the `tower` CLI build, and report what is missing. Installs only rustup toolchains/targets/components. Run explicitly as /bootstrap on a fresh clone.
argument-hint: "[host|firmware|hil]"
disable-model-invocation: true
allowed-tools:
  - Bash(git log -1 --oneline)
  - Bash(git status --short)
  - Bash(rustup --version)
  - Bash(rustup toolchain list)
  - Bash(rustup show active-toolchain)
  - Bash(rustup target list --installed *)
  - Bash(rustup component list --installed *)
  - Bash(rustup toolchain install *)
  - Bash(rustup target add *)
  - Bash(rustup component add *)
  - Bash(cargo --version)
  - Bash(cargo install --list)
  - Bash(cargo build --locked --release -p tower-cli)
  - Bash(just --version)
  - Bash(tower --version)
  - Bash(probe-rs --version)
  - Bash(python3 --version)
  - Bash(which *)
  - Bash(ls *)
  - Read
  - Grep
---

# Bootstrap the TOWER monorepo

You are checking a fresh clone of the **TOWER monorepo** (`hardwario/tower`) so the user can work
on the whole stack from this one session. There are no submodules: every component is a
directory here (`crates/*`, `firmware/`, `cli/`, `hil/`; see `AGENTS.md`).

Optional argument `$ARGUMENTS`: `host` (crates/*, cli, hil), `firmware`, or `hil` limits the
checks to what that part needs. Bootstrap verifies setup; to run the gates, use `/build`.

Work through these steps and report a concise status table at the end. Stop and surface any
error rather than pushing past it. Bootstrap never installs cargo tools itself; it reports what
is missing with the exact install command and lets the user decide.

## 1. The checkout

`git log -1 --oneline` and `git status --short`: report the commit and whether the tree is
clean (uncommitted work is fine, just mention it).

## 2. Rust toolchain (read, don't assume)

Read the root `rust-toolchain.toml` (channel + components + targets): it is the one toolchain
for both workspaces, and CI installs exactly that channel. Then:
- `rustup --version`: rustup must be present (https://rustup.rs).
- `rustup toolchain list`: the pinned channel must be installed; if not:
  `rustup toolchain install <channel> --profile minimal --component <components...> --target <targets...>`
  (taken from the file: explicit, rather than relying on rustup's auto-install).
- `rustup target list --installed --toolchain <channel>` and
  `rustup component list --installed --toolchain <channel>`: every `targets` / `components`
  entry must be present (`rustfmt`, `clippy`, `llvm-tools`, `thumbv6m-none-eabi`);
  `rustup target add --toolchain <channel> <triple>` /
  `rustup component add --toolchain <channel> <name>` for any that are missing.
- Linux only: `serialport` (tower-flasher, cli, hil) needs the `libudev` dev headers; if a build
  fails there, the hint is `libudev-dev` + `pkg-config`. The CLI also builds OpenSSL from source
  (`openssl/vendored`, its MQTT TLS stack on Linux): that needs `perl`, `make` and a C compiler.

## 3. Tools

- `just`: the task runner for every gate (`just --version`; CI uses 1.59.0). Install:
  `cargo install --locked just` or a package manager.
- `python3` >= 3.11: the firmware RAM-budget guard and `just fw lists`.
- **firmware** (see `firmware/.cargo/config.toml` + `firmware/justfile`):
  - `flip-link`: the **linker** set in `firmware/.cargo/config.toml` (stack-overflow
    protection); without it nothing links. `which flip-link`; install:
    `cargo install --locked flip-link`.
  - `cargo-binutils`: `just fw build` / `just fw size` run `cargo objcopy` / `cargo size`
    (check `cargo install --list`); install: `cargo install --locked cargo-binutils`. The
    `llvm-tools` component they drive comes from `rust-toolchain.toml`.
  - Optional: `probe-rs` (`cargo install --locked probe-rs-tools`), only for SWD flashing
    (`cargo run`, the `runner` in `firmware/.cargo/config.toml`) and hil's power group; UART
    flashing via `tower` does not need it.
- `cargo-audit` (for `just audit` / `just check`): `cargo install --list`; install:
  `cargo install --locked cargo-audit`.

## 4. The `tower` CLI

`just fw flash` / `just fw run` and the HIL harness drive the board through the `tower` CLI on
`PATH`. Check that it builds from this checkout:

```bash
cargo build --locked --release -p tower-cli
```

Then `tower --version`: if a `tower` is on `PATH`, compare its version with `cli/Cargo.toml`. An
installed binary built from another checkout speaks another schema (it reports a schema
mismatch, exit 125, against freshly flashed firmware): suggest `just install-cli`
(`cargo install --locked --path cli`) whenever it is missing or stale. Don't install it yourself.

## 5. hil (only if in scope)

`just`, the `tower` CLI and `python3` as above; the power group also needs `probe-rs` and a
`hil/.venv` with the PPK2 deps (`hil/docs/bench.md`). Report, don't install. Never run the
bench recipes.

## 6. Report

Read the root `AGENTS.md` (and, for a scoped run, that component's `AGENTS.md`) so you carry the
house rules into this session. Print a table: item | required for | found (version) | OK? | fix
command. Then state in one line whether the checkout is ready and what to do next (`/build` to
run the gates, `just fw build example blinky` for a first firmware image).
