---
name: build
description: Compile the TOWER ecosystem in dependency order (protocol → jolt → cli → firmware → hil), or one repo, against the committed lockfiles; stops at the first failure. Hardware-free, never flashes. Use after /sync, before /pin, or to check a cross-repo change compiles.
argument-hint: "[firmware|cli|protocol|jolt|hil] [--release] [example <name> | app <name>]"
allowed-tools:
  - Bash(cargo build --locked --manifest-path protocol/Cargo.toml*)
  - Bash(cargo build --locked --manifest-path jolt/Cargo.toml*)
  - Bash(cargo build --locked --manifest-path cli/Cargo.toml*)
  - Bash(cargo test --locked --manifest-path hil/Cargo.toml --no-run*)
  - Bash(just -f firmware/justfile check)
  - Bash(just -f firmware/justfile check --release)
  - Bash(just -f firmware/justfile examples)
  - Bash(just -f firmware/justfile apps)
  - Bash(git -C protocol log -1 --oneline)
  - Bash(git -C jolt log -1 --oneline)
  - Bash(git -C cli log -1 --oneline)
  - Bash(git -C firmware log -1 --oneline)
  - Bash(git -C hil log -1 --oneline)
  - Bash(ls *)
  - Read
  - Grep
---

# Build the TOWER ecosystem

Compile the submodules in **dependency order** so a failure points at the real culprit:
**protocol → jolt → cli → firmware → hil** (`protocol` is the shared crate
`firmware`+`cli`+`hil` depend on; `cli` and `hil` also link `jolt`). Assumes submodules are
already checked out; if not, run `/bootstrap` first.

Argument `$ARGUMENTS`:
- a repo name (`protocol` | `jolt` | `cli` | `firmware` | `hil`) builds **only** that repo (its
  in-tree dependencies still come from the pinned git tags, not the sibling submodules, unless a
  local `paths` override is active);
- `--release` builds optimized;
- `example <name>` or `app <name>` selects which firmware artifact to build (see below).

Every cargo invocation is `--locked`, like CI: the committed `Cargo.lock` is what gets built. A
"lock file needs to be updated" error is a real finding (a manifest changed without its lock,
e.g. half of a dependency bump), not something to paper over by dropping `--locked`.

Build each in-scope repo and **stop at the first failure**, reporting the exact command and the
compiler error. If `protocol` fails, say so plainly: `firmware`, `cli` and `hil` will almost
certainly fail downstream, so don't bother building them.

## protocol: the shared crate

```bash
cargo build --locked --manifest-path protocol/Cargo.toml
```

It must also stay embedded-buildable (the firmware links it on the MCU):

```bash
cargo build --locked --manifest-path protocol/Cargo.toml --target thumbv6m-none-eabi
```

## jolt: host UART flasher (lib + bin)

```bash
cargo build --locked --manifest-path jolt/Cargo.toml
```

## cli: host tool (`tower` binary)

```bash
cargo build --locked --manifest-path cli/Cargo.toml
```

(Linux: needs `libudev-dev` + `pkg-config`; not required on macOS.)

## firmware: embedded, via `just` (never `--manifest-path` from the root)

Firmware is `no_std` for `thumbv6m-none-eabi` and **links with flip-link**, both configured in
`firmware/.cargo/config.toml`. Cargo discovers that file by walking up from the CURRENT
directory, not from `--manifest-path`, so `cargo build --manifest-path firmware/Cargo.toml` from
this root silently builds for the HOST (or, with `--target`, links the apps WITHOUT flip-link's
stack-overflow protection): a misleading green. `just -f firmware/justfile …` runs its recipes
inside `firmware/`, so the config applies. Building does **not** need hardware.

- **Default (no `example`/`app` given):** compile the library + every product app + every
  example for the MCU, the same command CI's embedded build runs (needs `flip-link` on `PATH`;
  `/bootstrap` checks it):
  ```bash
  just -f firmware/justfile check
  ```
  (`check` already builds `--release`.)
- **A flashable artifact:** the `build` recipe links + objcopies to `firmware/target/firmware.bin`
  (needs `cargo-binutils`). List names first if unsure:
  ```bash
  just -f firmware/justfile examples      # or: just -f firmware/justfile apps
  just -f firmware/justfile build example <name>
  just -f firmware/justfile build app <name>
  ```
  `build` is deliberately not pre-approved (just accepts several recipes in one command line,
  so an approval pattern for it could chain `flash`); the user confirms it. Never run
  `flash`/`run` here.

## hil: HIL bench harness (host crate)

Compile-check only: the hardware tests are `#[ignore]`d and MUST NOT run here (they flash the
bench). `--features power` also compiles the feature-gated power group, like CI:

```bash
cargo test --locked --manifest-path hil/Cargo.toml --no-run --features power
```

(Linux: needs `libudev-dev` + `pkg-config`, like `cli`/`jolt`.)

Apply `--release` to the cargo invocations when requested (the firmware `check`/`build` recipes
already produce release builds).

## Report

Print a table: repo | command run | result (✅ / ❌) | notes . On success, end with a one-liner
that the ecosystem compiles and suggest `/pin` to freeze the known-good set if this followed a
`/sync`. Do not commit, push, or flash anything.
