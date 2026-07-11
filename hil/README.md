# tower-hil

Hardware-in-the-loop (HIL) test harness for the [HARDWARIO TOWER firmware
SDK](https://github.com/hardwario/tower-firmware). It drives a real bench — a **TOWER Core
Module** (SEGGER J-Link SWD + Nordic PPK2) and a **TOWER Radio Dongle** (USB) — and asserts
smoke, radio, extended, and power behaviour against the firmware's **framed console decoded
natively** with [`tower-protocol`](https://github.com/hardwario/tower-protocol): tests match on
typed `Log`/`Event` frames and on sequence gaps, not `strings | grep`.

**Bench inventory, cabling, and setup live in [`docs/bench.md`](docs/bench.md)** — start there
to assemble the bench from scratch. The test *catalogue* (what each firmware example asserts,
the example×role matrix) is documented in tower-firmware's `docs/test-plan.md`.

## Layout: where the firmware comes from

The harness **builds the images it flashes** from a `tower-firmware` checkout. By default it
looks **next to this repo** (`../firmware` — the [TOWER control
plane](https://github.com/hardwario/tower) layout, where `hil/` and `firmware/` are sibling
submodules). Any other layout:

```sh
TOWER_FIRMWARE_DIR=/path/to/tower-firmware just hil
```

## Running

```sh
cargo test        # no hardware: compiles every #[ignore]d bench test, runs only host unit tests
just hil          # bench: smoke + radio + extended + gateway groups (Dongle + Core over UART bootloader)
just hil-power    # bench: STOP-floor current measurement (J-Link + PPK2, FTDI UNPLUGGED)
just hil-full     # bench: everything, on the fully-cabled bench
```

Prerequisites: the [`tower` CLI](https://github.com/hardwario/tower-cli) and
[`just`](https://just.systems) on `PATH`; the power group also needs
[`probe-rs`](https://probe.rs) and `python3` (the `ppk2d.py` PPK2 sidecar). Linux builds need
`libudev-dev` + `pkg-config` (the `serialport` dependency).

The bench roster (which serial port is the Core, which is the Dongle, the PPK2 supply voltage)
lives in **`hil.toml`** and is re-resolved against the live `tower devices` roster at startup —
a missing board fails fast with instructions instead of hanging on a dead port.

`--test-threads=1` (baked into the `just` recipes) is load-bearing: the serial ports are
exclusive, so bench tests must not run concurrently.

## The golden rule (wire-format lockstep)

This crate pins `tower-protocol` by git tag — the **same tag** `tower-firmware` and `tower-cli`
pin. postcard is not self-describing: a mismatched pin silently mis-decodes the very frames the
tests assert on. Pin bumps happen as one coordinated change-set across all three consumers,
driven from the [control plane](https://github.com/hardwario/tower) (`/lockstep` checks it).

## License

MIT © 2026 HARDWARIO a.s.
