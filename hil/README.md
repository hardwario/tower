# tower-hil

Hardware-in-the-loop (HIL) test harness for the [HARDWARIO TOWER firmware
SDK](../firmware) (`hil/` in the [hardwario/tower](https://github.com/hardwario/tower)
monorepo). It drives a real bench (a **TOWER Core Module** with SEGGER J-Link SWD + Nordic
PPK2, and a **TOWER Radio Dongle** on USB) and asserts smoke, radio, extended, and power
behaviour against the firmware's **framed console decoded natively** with
[`tower-protocol`](../crates/protocol): tests match on typed `Log`/`Event` frames and on
sequence gaps, not `strings | grep`.

**Bench inventory, cabling, and setup live in [`docs/bench.md`](docs/bench.md)**; start there
to assemble the bench from scratch. The test *catalogue* (what each firmware example asserts,
the example×role matrix) is documented in the firmware's
[`docs/test-plan.md`](../firmware/docs/test-plan.md).

## Layout: where the firmware comes from

The harness **builds the images it flashes** from the firmware workspace, by default its
sibling **`../firmware`** in this repository (`firmware_dir()` in `src/lib.rs`), with that
directory's own `just build` recipes. To test images from another checkout:

```sh
TOWER_FIRMWARE_DIR=/path/to/tower/firmware just hil
```

## Running

Run these inside `hil/`, or from the repo root as `just hil <recipe>` (e.g. `just hil check`).

```sh
just check        # no hardware: compiles EVERY bench group incl. the feature-gated power group (= CI)
cargo test        # no hardware: runs only host unit tests; compiles the #[ignore]d smoke/radio/
                  # extended/gateway groups, but NOT power (that needs `--features power`);
                  # from the repo root: cargo test -p tower-hil
just hil          # bench: smoke + radio + extended + gateway groups (Dongle + Core over UART bootloader)
just hil-power    # bench: STOP-floor current measurement (J-Link + PPK2, no USB power; see below)
just hil-full     # bench: everything, on the fully-cabled bench
```

Prerequisites: the [`tower` CLI](../cli) (`just install-cli` at the repo root) and
[`just`](https://just.systems) on `PATH`; the power group also needs
[`probe-rs`](https://probe.rs) and `python3` (the `ppk2d.py` PPK2 sidecar; real hardware needs
its `.venv`, see [`docs/bench.md`](docs/bench.md)). The Rust toolchain (1.99.1) is pinned in the
repo-root `rust-toolchain.toml`. Linux builds need `libudev-dev` + `pkg-config` (the `serialport`
dependency). The images under test are built by the firmware's own `just` recipes, so its
prerequisites (`flip-link`, `cargo-binutils`) apply too; see the [firmware
README](../firmware/README.md).

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

## The wire format (one source tree)

The harness depends on `tower-protocol` **by path** (the root workspace's `crates/protocol`),
the very source the firmware and the `tower` CLI build. postcard is not self-describing, so a
schema mismatch would silently mis-decode the frames the tests assert on; with one source tree
that cannot happen at build time. It can still happen at run time against a board flashed from
another checkout (or built from one via `TOWER_FIRMWARE_DIR`): every console frame carries the
firmware's schema tag, and the harness fails fast on a foreign one ("the device's console frames
carry schema tag ..."); rebuild and reflash from the same checkout. The reset pulses come from
`tower-flasher` (`crates/flasher`), also by path.

## License

MIT © 2026 HARDWARIO a.s. (see the repo-root `LICENSE`).
