# The HIL bench — inventory, cabling, and setup

This is the in-repo source of truth for physically assembling and configuring the bench that
`just hil` / `just hil-power` / `just hil-full` drive. The *test catalogue* (what each firmware
example asserts, the full example×role matrix) lives in tower-firmware's `docs/test-plan.md`;
this file is what that plan assumes you already have on the desk.

## Inventory

| Item | Purpose |
|---|---|
| TOWER **Core Module** (STM32L083CZ) | The `[core]` roster slot: radio NODE for RF tests, the power-measurement target |
| TOWER **Radio Dongle** | The `[dongle]` roster slot: radio GATEWAY / smoke-KAT target, USB-powered |
| FTDI USB-UART bridge (per board) | Console (USART1) + UART-bootloader flashing via `tower flash` |
| SEGGER **J-Link** | SWD flashing of the Core via `probe-rs` (power tests only — works with the FTDI unplugged) |
| Nordic **PPK2** | Source-meter: supplies the Core's VDD *and* measures its current (power tests only) |

The smoke/radio/extended groups need only the two boards on their FTDI ports. The **power**
group needs the J-Link + PPK2 on the Core and the **FTDI unplugged** from it.

## Topology

```
                     USB hub (host)
      ┌──────────┬──────────────┬────────────┬──────────┐
      │          │              │            │          │
   FTDI #1    FTDI #2        J-Link        PPK2      (host USB)
      │          │              │            │
      │       console        SWD 10-pin   VOUT/GND
      │       + NRST/BOOT0     │            │
      ▼          ▼             ▼            ▼
  ┌────────┐ ┌──────────────────────────────────┐
  │ Radio  │ │        TOWER Core Module          │
  │ Dongle │ │  (FTDI unplugged for power runs)  │
  └────────┘ └──────────────────────────────────┘
```

## Cabling, per instrument

**FTDI ↔ board (console + bootloader flashing)**
- USART1 on PA9 (TX) / PA10 (RX), 115200 8N1. The link is COBS+CRC framed — a raw serial
  monitor shows binary; that is expected.
- NRST and BOOT0 are driven from the FTDI's **aux** lines (not DTR/RTS), so merely opening
  the port does **not** reset the board. `tower flash` / `tower reset` pulse them.

**J-Link ↔ Core (SWD, power group)**
- Standard 10-pin Cortex debug header: SWDIO, SWCLK, GND, VTref. **nRESET is not wired** on
  this bench — see "Reflashing a sleeping Core" below for why that matters.
- Flashing goes through `probe-rs` (`--chip STM32L083CZTx`); the harness resets+detaches after
  download so the core runs free (a halted core draws mA).

**PPK2 ↔ Core (power group)**
- PPK2 in **source-meter mode**: VOUT → Core VDD, GND → GND. It both powers the board and
  measures the current.
- Supply is pinned at **1.8 V** (`hil.toml` `[ppk2] supply_mv = 1800`) — not 3 V. Near ~2 V a
  board-level consumer with an LED-like knee stops conducting, so 1.8 V is the honest STOP
  floor; 3 V readings are inflated by ~200 µA.
- The Core's FTDI must be **unplugged**: VBUS present keeps the SDK console alive, which
  inhibits STOP *by design*. The power test detects a still-answering console and skips with
  an "unplug the FTDI" message rather than record a false ~mA reading.

## Software setup (fresh machine)

```sh
# Rust + the harness
rustup toolchain install stable
cargo test --manifest-path hil/Cargo.toml --no-run   # compile-check, no hardware

# Host tools on PATH
#  - tower  (tower-cli release archive, or: cargo build --release in cli/)
#  - just   (https://just.systems)
#  - probe-rs (power group only): cargo install probe-rs-tools
#  - python3 (power group only; runs the ppk2d.py sidecar)

# Linux only: the serialport crate needs
sudo apt-get install -y libudev-dev pkg-config
```

The firmware images are built from a `tower-firmware` checkout — default `../firmware`
(the control-plane layout), override with `TOWER_FIRMWARE_DIR`.

> **PPK2 sidecar status:** `ppk2d.py` speaks line-JSON to the harness and encodes the
> measurement *policy* (power-cycle before every average, 1.8 V ceiling, reject >50 mA
> desync garbage). It drives a **real PPK2** via Nordic's `ppk2-api` (source-measure,
> per-unit calibration), and falls back to a modelled stub when no PPK2 / `ppk2-api` is
> present — so CI stays hardware-free while a cabled bench measures real silicon. The
> confounder policy is enforced identically on both paths. To enable real measurement,
> create the sibling venv the sidecar auto-execs into:
> `python3 -m venv .venv && .venv/bin/pip install ppk2-api pyserial` (in `hil/`; it's
> gitignored). Everything else about the bench (guards, skips, roster) behaves identically.

## Roster (`hil.toml`)

Serial-port names **re-enumerate between sessions** (`usbserial-110` can come back as
`usbserial-2110`), so `hil.toml` is only the starting guess. At startup the harness re-resolves
the roster against the live `tower devices` output and **fails fast with instructions** if a
board is missing — update `hil.toml` to the current names when that fires. Two boards must be
distinguishable; if the roster is ambiguous the resolve step says so rather than guessing.

## Running

```sh
just hil          # smoke + radio + extended: two boards on FTDI, no J-Link/PPK2 needed
just hil-power    # STOP-floor measurement: Core on J-Link + PPK2, FTDI UNPLUGGED
just hil-full     # everything, fully-cabled bench
```

`--test-threads=1` is baked into the recipes and load-bearing: the ports are exclusive.

## Reflashing a sleeping Core (power bench)

An STM32L0 in STOP cannot be halted over SWD, and nRESET is not wired here. The procedure the
harness uses (and you'd use manually):

1. Power-cycle via the PPK2 (`cycle`).
2. Attach probe-rs **during the boot RUN window** (the LSE startup + boot spin gives ~12 s).
3. After download, reset+detach so the core runs free.
4. Power-cycle once more before measuring — a probe attach leaves the debug power domain up
   (+~200 µA in STOP) until a clean cycle.

## The three power-measurement confounders

Every number the power group reports has these handled; if you measure by hand, handle them too:

1. **Debug-domain residual**: any SWD attach adds ~200 µA in STOP until a PPK2-only power
   cycle. Never measure without cycling after a flash.
2. **The ~2 V knee**: measure at 1.8 V, not 3 V (see PPK2 cabling above).
3. **PPK2 CDC desync**: concurrent SWD traffic corrupts the PPK2 byte stream into tens-of-mA
   garbage. Never sample mid-flash; readings above 50 mA are rejected as noise.

Reference clean floor on this bench: **~14.5 µA @ 1.8 V** for the STOP example (including
~11 µA of free-running sensors); a J-Link left attached in a clean non-debug state adds ~30 µA.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Roster resolve fails at startup | Ports re-enumerated — update `hil.toml` to the names `tower devices` shows |
| `tower flash` Write-Memory timeouts, ports renaming mid-run | Concurrent FTDI flashing on one USB bus — the harness flashes sequentially by design; don't parallelize it |
| Power test skips with "unplug the FTDI" | Working as intended: VBUS inhibits STOP; unplug the Core's FTDI |
| STOP reading ~200 µA too high | Debug domain left up — power-cycle via PPK2 after the last probe-rs contact |
| PPK2 reports tens of mA | CDC desync garbage — re-cycle and re-average; never sample during SWD traffic |
| Bench test hangs on a dead port | Should not happen (resolve fails fast) — check the board's USB cable/power |
