# tower-hil: working notes for coding agents

Std host crate: the HIL bench harness for the TOWER firmware SDK (`hil/` in the hardwario/tower
monorepo, a member of the root host workspace; read the root `AGENTS.md` first). A plain
`cargo test` (here, or `cargo test -p tower-hil` at the root) compiles the `#[ignore]`d hardware
tests and runs only the host unit tests, safe anywhere. The `just hil` / `just hil-power` /
`just hil-full` recipes (also `just hil <recipe>` from the repo root) opt INTO the bench run and
**flash real hardware**: never run them unless the user asks and the bench is cabled.

## Load-bearing invariants

- **`--test-threads=1`** in every bench recipe: the serial ports are exclusive.
- **The `power_` name filter** in `just hil-power`: `--features power` only ADDS power.rs;
  without the filter the run would also pull in smoke/radio/extended, whose `tower flash` of
  the Core times out on a power bench (it is PPK2-powered behind a data-only FTDI, so it is
  unpowered unless the sidecar sources it).
- **Firmware discovery** (`firmware_dir()` in src/lib.rs): default `../firmware`, the sibling
  firmware workspace in this repository; `TOWER_FIRMWARE_DIR` overrides. The harness builds
  images via the firmware's `just build example <name>` (and `just build app <name>` for the
  gateway group's product firmwares; the power group builds `lowpower` via
  `cargo … --example`), always run inside that directory so its `.cargo/config.toml`
  (thumbv6m + flip-link) applies.
- **hil.toml** is bench-local operator config (port serials re-enumerate); don't assert on
  its concrete values in tests.

## Shared crates (path dependencies)

`tower-protocol` (`crates/protocol`) and `tower-flasher` (`crates/flasher`, the NRST/BOOT0 reset
pulses) come from the root workspace by path. The harness decodes exactly the schema the
firmware and the `tower` CLI build, so a wire mismatch cannot arise at build time; a protocol
change updates the harness in the same commit (see `crates/protocol/AGENTS.md`, "Changing the
wire"). At run time a board flashed from another checkout carries another schema tag in every
console frame, and `Console::next` fails fast with the lockstep message.

## Checks

```sh
just check                                                   # = cargo test --locked --no-run --features power
cargo clippy -p tower-hil --all-targets --features power -- -D warnings   # from the repo root
```

The repo-root `just check` runs both (CI's `host` job too).

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.

## Git workflow

- Part of the hardwario/tower monorepo, developed straight on `main`; no feature branches or PRs
  unless the user asks.
- Commit and push only when the user requests it.
- MIT © 2026 HARDWARIO a.s. Design-rationale comments (confounders, reset sequencing,
  TIOCEXCL handling) are hard-won; don't strip them.
