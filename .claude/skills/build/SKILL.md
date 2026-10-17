---
name: build
description: Run the TOWER monorepo gates (the root `just check`, or one part of it: host, firmware, wire, audit) against the committed lockfiles; stops at the first failure. Hardware-free, never flashes. Use after a change, before committing, or to check that a cross-component change compiles.
argument-hint: "[host|firmware|wire|audit] [example <name> | app <name>]"
allowed-tools:
  - Bash(just check)
  - Bash(just check-host)
  - Bash(just check-firmware)
  - Bash(just check-wire)
  - Bash(just audit)
  - Bash(just test)
  - Bash(just doc)
  - Bash(just fw ci)
  - Bash(just fw check)
  - Bash(just fw examples)
  - Bash(just fw apps)
  - Bash(just hil check)
  - Bash(git log -1 --oneline)
  - Bash(git status --short)
  - Bash(ls *)
  - Read
  - Grep
---

# Run the TOWER gates

The root `justfile` holds every gate CI runs (`.github/workflows/ci.yml`), for both Cargo
workspaces (the host workspace at the root, the firmware workspace in `firmware/`). Run them
through `just` so the firmware's `.cargo/config.toml` (thumbv6m target + flip-link linker)
applies: never `cargo build --manifest-path firmware/...` from the root, which silently builds
for the host or links without flip-link's stack-overflow protection.

Argument `$ARGUMENTS`:
- nothing: `just check`, every gate, in this order (stop at the first failure):
  1. `just check-host`: fmt, clippy `-D warnings` (workspace + `tower-hil --features power`),
     `cargo test --workspace`, the HIL harness compile, the no_std build of the shared crates,
     rustdoc;
  2. `just check-firmware`: `just fw ci` (fmt, MCU build, RAM budget, clippy + the five role
     variants, rustdoc, example/app list drift) and `just fw build example blinky`;
  3. `just check-wire`: the wire-bump guard + its self-test;
  4. `just audit`: `cargo audit` on both lockfiles (needs network for the advisory DB);
- `host`, `firmware`, `wire` or `audit`: only that part (`just check-host`, ...);
- `example <name>` or `app <name>`: build one flashable firmware artifact,
  `just fw build example <name>` / `just fw build app <name>` (needs cargo-binutils; output
  `firmware/target/firmware.bin`). List names with `just fw examples` / `just fw apps`.
  `build` is deliberately not pre-approved (just accepts several recipes in one command line,
  so an approval pattern for it could chain `flash`); the user confirms it.

Every cargo invocation in the recipes is `--locked`, like CI: the committed `Cargo.lock` files
are what get built. A "lock file needs to be updated" error is a real finding (a manifest
changed without its lock), not something to paper over by dropping `--locked`.

If a step fails, report the exact command and the compiler / tool error. If `crates/protocol`
fails to build, say so plainly: the firmware, the CLI and the HIL harness all depend on it, so
their failures are downstream.

Never run `just fw flash` / `just fw run` or the hil bench recipes (`just hil hil`, `hil-power`,
`hil-full`): they flash real hardware. Missing tools (`flip-link`, `cargo-binutils`,
`cargo-audit`, `just`): point at `/bootstrap`.

## Report

Print a table: gate | command run | result (pass / FAIL) | notes. On success, end with a
one-liner that every gate is green. Do not commit, push, tag or flash anything.
