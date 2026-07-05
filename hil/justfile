# HARDWARIO TOWER HIL harness — task runner (https://just.systems)
#
# The harness drives the real bench: a TOWER Core Module (J-Link SWD + Nordic PPK2) and a
# TOWER Radio Dongle (USB). It builds the images under test from a tower-firmware checkout —
# default `../firmware` (the TOWER control-plane layout, where `hil/` and `firmware/` are
# sibling submodules); override with TOWER_FIRMWARE_DIR. The bench roster lives in hil.toml
# (re-resolved against `tower devices` at startup). Needs the `tower` CLI and `just` on PATH;
# the power group also needs probe-rs + python3 (the ppk2d.py sidecar).
#
# This crate is a plain host crate: `cargo test` builds for the native host, COMPILES the
# `#[ignore]`d hardware tests, and runs none — `--ignored` (below) opts INTO the bench run.
#
# `--test-threads=1` is LOAD-BEARING: the serial ports are exclusive, so tests must not run
# concurrently.

# List the available recipes.
default:
    @just --list

# Run the smoke + radio + extended HIL groups on the bench (needs the Dongle + Core; NOT run in
# CI). The `power` group is compiled out (no `power` feature), so this never touches the PPK2.
# `--no-fail-fast`: one red test binary must not skip the remaining groups — a 2026-07-05 run
# lost the whole smoke+radio pass to a single RF flake in extended.
hil *args:
    cargo test --no-fail-fast -- --ignored --test-threads=1 {{args}}

# Run ONLY the feature-gated power HIL group (needs the Core on J-Link + PPK2, FTDI UNPLUGGED).
# The `power_` name filter is load-bearing: `--features power` merely ADDS power.rs — without the
# filter, `--ignored` would ALSO run the smoke/radio/extended groups, whose `tower flash` of the
# FTDI-detached Core times out on a power bench. `hil-full` is the unfiltered "everything" run.
hil-power *args:
    cargo test --no-fail-fast --features power -- --ignored --test-threads=1 power_ {{args}}

# Run every HIL group (smoke + radio + extended + power) on the fully-cabled bench.
hil-full *args:
    cargo test --no-fail-fast --features power -- --ignored --test-threads=1 {{args}}

# Compile the harness + every test group (incl. power) without touching hardware — what CI runs.
check:
    cargo test --no-run --features power

# Remove build artifacts.
clean:
    cargo clean
