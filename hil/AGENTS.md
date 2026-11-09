# tower-hil: rules for agents

The hardware-in-the-loop bench harness, a member of the root host workspace. Its guides:
[docs/hil/hil.md](../docs/hil/hil.md) (the harness) and [docs/hil/bench.md](../docs/hil/bench.md)
(the bench).

A plain `cargo test` (here, or `cargo test -p tower-hil` at the root) compiles the `#[ignore]`d
hardware tests and runs only the host unit tests, safe anywhere. The `just bench` / `just
bench-power` / `just bench-full` recipes (`just hil <recipe>` from the repo root) opt INTO the
bench run and **flash real hardware**: never run them unless the user asks and the bench is
cabled.

## Load-bearing invariants

- **`--test-threads=1`** in every bench recipe: the serial ports are exclusive.
- **The `power_` name filter** in `just bench-power`: `--features power` only ADDS power.rs;
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
- **The wire:** a protocol change updates the harness in the same commit. A board flashed from
  another checkout carries another schema tag in every console frame, and `Console::next` fails
  fast with the lockstep message.
- The design-rationale comments (confounders, reset sequencing, TIOCEXCL handling) are hard-won;
  don't strip them.

## Checks

```sh
just check                                                   # = cargo test --locked --no-run --features power
cargo clippy -p tower-hil --all-targets --features power -- -D warnings   # from the repo root
```

The repo-root `just check` runs both (CI's `host` job too).
