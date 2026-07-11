# tower-hil — working notes for Claude

Std host crate: the HIL bench harness for the TOWER firmware SDK (extracted from
tower-firmware's `tools/hil`). A plain `cargo test` compiles the `#[ignore]`d hardware tests
and runs only the host unit tests — safe anywhere. The `just hil` / `just hil-power` /
`just hil-full` recipes opt INTO the bench run and **flash real hardware**: never run them
unless the user asks and the bench is cabled.

## Load-bearing invariants

- **`--test-threads=1`** in every bench recipe: the serial ports are exclusive.
- **The `power_` name filter** in `just hil-power`: `--features power` only ADDS power.rs;
  without the filter the run would also pull in smoke/radio/extended, whose `tower flash` of
  the FTDI-detached Core times out on a power bench.
- **Firmware checkout discovery** (`firmware_dir()` in src/lib.rs): default `../firmware`
  (the control-plane layout), `TOWER_FIRMWARE_DIR` overrides. The harness builds images via
  the firmware repo's `just build example <name>` (and `just build app <name>` for the gateway
  group's product firmwares; the power group builds `lowpower` via `cargo … --example`).
- **hil.toml** is bench-local operator config (port serials re-enumerate); don't assert on
  its concrete values in tests.

## The golden rule (wire-format lockstep)

`tower-protocol` is pinned by git tag in `Cargo.toml` and MUST carry the same tag as
tower-firmware (two manifests) and tower-cli. postcard is not self-describing — a mismatch
silently mis-decodes the frames this harness asserts on. Bump the pin only as part of the
coordinated change-set driven from the control plane (github.com/hardwario/tower, `/lockstep`
+ the protocol-bump runbook in its CLAUDE.md). The `jolt` tag pin is an ordinary dependency
(a mismatch is a compile error, not an interop hazard).

## Git workflow

- Developed straight on `main`; no feature branches or PRs unless the user asks.
- Commit and push only when the user requests it.
- MIT © 2026 HARDWARIO a.s. Design-rationale comments (confounders, reset sequencing,
  TIOCEXCL handling) are hard-won — don't strip them.
