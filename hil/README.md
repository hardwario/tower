# tower-hil

Hardware-in-the-loop (HIL) test harness for the [HARDWARIO TOWER firmware
SDK](https://github.com/hardwario/tower-firmware). It drives a real bench (a **TOWER Core
Module** with SEGGER J-Link SWD + Nordic PPK2, and a **TOWER Radio Dongle** on USB) and asserts
smoke, radio, extended, and power behaviour against the firmware's **framed console decoded
natively** with [`tower-protocol`](https://github.com/hardwario/tower-protocol): tests match on
typed `Log`/`Event` frames and on sequence gaps, not `strings | grep`.

**Bench inventory, cabling, and setup live in [`docs/bench.md`](docs/bench.md)**; start there
to assemble the bench from scratch. The test *catalogue* (what each firmware example asserts,
the example×role matrix) is documented in tower-firmware's `docs/test-plan.md`.

## Layout: where the firmware comes from

The harness **builds the images it flashes** from a `tower-firmware` checkout. By default it
looks **next to this repo** (`../firmware`, the [TOWER control
plane](https://github.com/hardwario/tower) layout, where `hil/` and `firmware/` are sibling
submodules). Any other layout:

```sh
TOWER_FIRMWARE_DIR=/path/to/tower-firmware just hil
```

## Running

```sh
just check        # no hardware: compiles EVERY bench group incl. the feature-gated power group (= CI)
cargo test        # no hardware: runs only host unit tests; compiles the #[ignore]d smoke/radio/
                  # extended/gateway groups, but NOT power (that needs `--features power`)
just hil          # bench: smoke + radio + extended + gateway groups (Dongle + Core over UART bootloader)
just hil-power    # bench: STOP-floor current measurement (J-Link + PPK2, no USB power; see below)
just hil-full     # bench: everything, on the fully-cabled bench
```

Prerequisites: the [`tower` CLI](https://github.com/hardwario/tower-cli) and
[`just`](https://just.systems) on `PATH`; the power group also needs
[`probe-rs`](https://probe.rs) and `python3` (the `ppk2d.py` PPK2 sidecar; real hardware needs
its `.venv`, see [`docs/bench.md`](docs/bench.md)). The Rust toolchain (1.99.1) is pinned in
`rust-toolchain.toml`. Linux builds need `libudev-dev` + `pkg-config` (the `serialport`
dependency). The images under test are built by the firmware checkout's own `just` recipes, so
its prerequisites (`flip-link`, `cargo-binutils`) apply too; see tower-firmware's README.

The bench roster (which serial port is the Core, which is the Dongle, the PPK2 supply voltage)
lives in **`hil.toml`** and is re-resolved against the live port roster and identity-checked
(board-type tag in the FTDI's USB serial, e.g. `hio-core-module-…` / `bc-usb-dongle-…`, or a
pinned `usb_serial`) before anything is flashed: a missing or wrong board fails fast with
instructions instead of hanging on a dead port or flashing the wrong device.

The power group requires a real PPK2 (the sidecar's stub is opt-in only, `TOWER_PPK2_STUB=1`)
and fails if the board reads as USB/VBUS-powered (median < 1 µA): keep only a data-only FTDI
attached during power runs.

`--test-threads=1` (baked into the `just` recipes) is load-bearing: the serial ports are
exclusive, so bench tests must not run concurrently.

## The golden rule (wire-format lockstep)

This crate pins `tower-protocol` by git tag: the **same tag** `tower-firmware` and `tower-cli`
pin. postcard is not self-describing: a mismatched pin silently mis-decodes the very frames the
tests assert on. Pin bumps happen as one coordinated change-set across all three consumers,
driven from the [control plane](https://github.com/hardwario/tower) (`/lockstep` checks it).
This repo's CI `lockstep` job checks out tower-firmware and tower-cli and runs the shared
checker (tower-firmware's `tools/protocol_pin_check.py`) across all three: every manifest pin,
the resolved `Cargo.lock` commits, and the tag's current upstream commit.

## License

MIT © 2026 HARDWARIO a.s.
