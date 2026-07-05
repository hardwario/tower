---
description: Compile the TOWER ecosystem in dependency order (protocol → jolt → cli → firmware → hil), or a single repo
argument-hint: "[firmware|cli|protocol|jolt|hil] [--release] [example <name> | app <name>]"
allowed-tools: Bash(cargo:*), Bash(just:*), Bash(git -C:*), Bash(ls:*), Read
---

# Build the TOWER ecosystem

Compile the submodules in **dependency order** so a failure points at the real
culprit: **protocol → jolt → cli → firmware → hil** (`protocol` is the shared crate
`firmware`+`cli`+`hil` depend on; `cli` and `hil` also link `jolt`). Assumes submodules
are already checked out — if not, run `/bootstrap` first.

Argument `$ARGUMENTS`:
- a repo name (`protocol` | `jolt` | `cli` | `firmware` | `hil`) builds **only** that repo
  (its in-tree dependencies still come from the pinned git tags, not the sibling
  submodules, unless a local `paths` override is active);
- `--release` builds optimized;
- `example <name>` or `app <name>` selects which firmware artifact to build (see below).

Build each in-scope repo and **stop at the first failure**, reporting the exact
command and the compiler error. If `protocol` fails, say so plainly — `firmware`
and `cli` will almost certainly fail downstream, so don't bother building them.

## protocol — the shared crate
```bash
cargo build --manifest-path protocol/Cargo.toml
```
For a fuller check (the crate must stay embedded-buildable), also:
```bash
cargo build --manifest-path protocol/Cargo.toml --target thumbv6m-none-eabi
```

## jolt — host UART flasher (lib + bin)
```bash
cargo build --manifest-path jolt/Cargo.toml
```

## cli — host tool (`tower` binary)
```bash
cargo build --manifest-path cli/Cargo.toml
```
(Linux: needs `libudev-dev` + `pkg-config`; not required on macOS.)

## firmware — embedded, via `just`
Firmware is `no_std` for `thumbv6m-none-eabi` with a custom runner — **do not** run a
bare `cargo build` expecting a flashable image. Building does **not** need hardware
(only flashing/running does).
- **Default (no `example`/`app` given):** compile-check the library for the embedded
  target — fast, deterministic, hardware-free. Pass `--target` **explicitly**: cargo discovers
  `.cargo/config.toml` by walking up from the CURRENT directory, not from `--manifest-path`, so
  from the control-plane root `firmware/.cargo/config.toml` (which sets `target = thumbv6m`) is
  NOT read — a bare `cargo build --manifest-path firmware/Cargo.toml` would build the HOST triple
  and report a misleading green that never touched thumbv6m:
  ```bash
  cargo build --manifest-path firmware/Cargo.toml --target thumbv6m-none-eabi
  ```
- **A flashable artifact:** use the repo's task runner (it links + objcopies to a
  `.bin`). List names first if unsure:
  ```bash
  just -f firmware/justfile examples      # or: just -f firmware/justfile apps
  just -f firmware/justfile build example <name>
  just -f firmware/justfile build app <name>
  ```

## hil — HIL bench harness (host crate)
Compile-check only — the hardware tests are `#[ignore]`d and MUST NOT run here (they
flash the bench):
```bash
cargo test --manifest-path hil/Cargo.toml --no-run
```
(Linux: needs `libudev-dev` + `pkg-config`, like `cli`/`jolt`.)

Apply `--release` to the cargo invocations when requested (the `just build` recipes
already produce release binaries).

## Report
Print a table: repo | command run | result (✅ / ❌) | notes . On success, end with a
one-liner that the ecosystem compiles and suggest `/pin` to freeze the known-good set
if this followed a `/sync`. Do not commit, push, or flash anything.
