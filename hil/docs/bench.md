# The HIL bench: inventory, cabling, and setup

This is the in-repo source of truth for physically assembling and configuring the bench that
`just bench` / `just bench-power` / `just bench-full` drive. The *test catalogue* (what each firmware
example asserts, the full example×role matrix) lives in the firmware's
[`docs/test-plan.md`](../../firmware/docs/test-plan.md); this file is what that plan assumes you
already have on the desk.

## Inventory

| Item | Purpose |
|---|---|
| TOWER **Core Module** (STM32L083CZ) | The `[core]` roster slot: radio NODE for RF tests, the power-measurement target |
| TOWER **Radio Dongle** | The `[dongle]` roster slot: radio GATEWAY / smoke-KAT target, USB-powered |
| FTDI USB-UART bridge (per board) | Console (USART1) + UART-bootloader flashing via `tower flash` |
| SEGGER **J-Link** | SWD flashing of the Core via `probe-rs` (power tests only; independent of the FTDI) |
| Nordic **PPK2** | Source-meter: supplies the Core's VDD *and* measures its current (power tests only) |

The smoke/radio/extended groups need only the two boards on their FTDI ports. The **power**
group needs the J-Link + PPK2 on the Core, the Core **PPK2-powered**, and **no VBUS** on its
USB: the supported setup is a **data-only** FTDI adapter (no VBUS line), which may stay plugged.

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
  │ Dongle │ │ (FTDI #2 data-only: no VBUS; the  │
  │        │ │   PPK2 is the Core's only supply) │
  └────────┘ └──────────────────────────────────┘
```

## Cabling, per instrument

**FTDI ↔ board (console + bootloader flashing)**
- USART1 on PA9 (TX) / PA10 (RX), 115200 8N1. The link is COBS+CRC framed, so a raw serial
  monitor shows binary; that is expected.
- NRST and BOOT0 hang off the FTDI's **RTS / DTR** modem lines through the board's transistor
  pair (tower-flasher / tower-cli wiring: RTS→NRST, DTR→BOOT0, a 1 µF cap on NRST makes the line
  *order* matter). Only RTS-asserted + DTR-released holds RESET; both asserted (what the OS drives on
  open, and what the harness re-drives explicitly) is the run state, so merely opening the port
  does **not** reset the board. `tower flash` / `tower reset` / `Console::reset_into_app` pulse
  them with tower-flasher's tuned sequence.

**J-Link ↔ Core (SWD, power group)**
- Standard 10-pin Cortex debug header: SWDIO, SWCLK, GND, VTref. **nRESET is not wired** on
  this bench; see "Reflashing a sleeping Core" below for why that matters.
- Flashing goes through `probe-rs` (`--chip STM32L083CZTx`); the harness resets+detaches after
  download so the core runs free (a halted core draws mA).

**PPK2 ↔ Core (power group)**
- PPK2 in **source-meter mode**: VOUT → Core VDD, GND → GND. It both powers the board and
  measures the current.
- Supply is pinned at **1.8 V** (`hil.toml` `[ppk2] supply_mv = 1800`). On this bench the true
  floor is honest at both rails: **5.1 µA @ 1.8 V, 6.1 µA @ 3.0 V** (firmware
  `examples/lowpower.rs`; the ~1 µA rise is ordinary CMOS leakage). One pinned rail keeps runs
  comparable, and keeps dark any board/adapter consumer with an LED-like ~2 V knee (an earlier
  rig read ~+200 µA at 3 V from one).
- **No VBUS on the Core's USB.** The supported power topology is a PPK2-powered Core whose FTDI
  adapter is **data-only** (no VBUS line): it may stay plugged during `just bench-power` (its port
  can be in the roster; the power test doesn't use it). A **VBUS-carrying** USB invalidates the
  run: VBUS back-feeds VDD through the board's LDO above the PPK2's rail (the PPK2 then sources
  ~0 and reads **~0.3 µA**, a reading that would pass any upper bound), and VBUS-sense high
  keeps the SDK console alive, which inhibits STOP *by design*. The power test's
  **plausibility floor** (median ≥ 1 µA, vs the true 5.1 µA) is the VBUS detector: it fails
  (never skips) with "board is probably USB/VBUS-powered (…); unplug the VBUS-carrying USB, or use
  a data-only FTDI adapter".

## Software setup (fresh machine)

```sh
# Rust + the harness (rustup installs the toolchain pinned by the repo-root rust-toolchain.toml)
just hil check        # at the repo root: compile-check every group incl. power, no hardware

# Host tools on PATH
#  - tower  (just install-cli at the repo root, or a cli-v* release archive)
#  - just   (https://just.systems)
#  - probe-rs (power group only): cargo install probe-rs-tools
#  - python3 (power group only; runs the ppk2d.py sidecar)

# Linux only: the serialport crate needs
sudo apt-get install -y libudev-dev pkg-config
```

The firmware images are built from the firmware workspace: default `../firmware` (the sibling
directory in this repository), override with `TOWER_FIRMWARE_DIR`.

> **PPK2 sidecar status:** `ppk2d.py` speaks line-JSON to the harness and encodes the
> measurement *policy* (power-cycle before every sample window, supply ceiling, reject >50 mA
> desync garbage). It drives a **real PPK2** via Nordic's `ppk2-api` (source-measure,
> per-unit calibration) and reports both the **median** (STOP floor) and the **mean**
> (average draw, wake spikes included) of each window. If no PPK2 initialises (not plugged
> in, `ppk2-api` missing, a broken `.venv` after a Python upgrade, a calibration flake), it
> says why and the power test **fails**; it never silently substitutes its modelled stub
> (a believable fake ~12 µA). The stub runs only on explicit opt-in, `TOWER_PPK2_STUB=1`,
> for plumbing work. CI never starts the sidecar. To enable real measurement, create the
> sibling venv the sidecar auto-execs into:
> `python3 -m venv .venv && .venv/bin/pip install ppk2-api pyserial` (in `hil/`; it's
> gitignored; recreate it after a Python upgrade).

## Roster (`hil.toml`)

Serial-port names **re-enumerate between sessions** (`usbserial-110` can come back as
`usbserial-2110`), so `hil.toml` is only the starting guess. At startup the harness re-resolves
the roster against the live port list (what `tower devices` prints, plus each port's USB serial
number) and **fails fast with instructions** (listing every port with its USB serial) if a
board is missing; update `hil.toml` to the current names when that fires.

Each slot's **identity** is checked before anything is flashed, and again right before every
`tower flash`. HARDWARIO programs each board's FTDI with a board-type USB serial number
(`hio-core-module-<id>`, `bc-usb-dongle-<rev>-<id>`), so the harness refuses a Radio Dongle in
the `[core]` slot (swapped cables) and vice versa, an untagged FTDI at a fixture path (some
other device that `tower flash` would erase), and one board resolving into both slots. Pin a
board by its exact USB serial (`usb_serial = "…"` under `[core]`/`[dongle]`) to make it immune
to re-enumeration, to break a tie, or to vouch for a board on a third-party adapter.

## Running

```sh
just bench        # smoke + radio + extended: two boards on FTDI, no J-Link/PPK2 needed
just bench-power  # STOP-floor measurement: Core on J-Link + PPK2, PPK2-powered (data-only FTDI)
just bench-full   # everything, fully-cabled bench
```

`--test-threads=1` is baked into the recipes and load-bearing: the ports are exclusive.

## Reflashing a sleeping Core (power bench)

An STM32L0 in STOP cannot be halted over SWD, and nRESET is not wired here (so probe-rs's
`--connect-under-reset` is inert on this bench). The procedure the harness uses (and you'd use
manually):

1. Build the image **first**: a release/LTO build takes tens of seconds and must not run
   between the power-cycle and the attach.
2. Power-cycle via the PPK2 (`cycle`, not just `on`: a supply left on means the core is long
   since asleep).
3. Attach probe-rs **immediately, during the post-power-on RUN window** (clock/LSE start-up,
   before the app's first sleep; short for `lowpower`, which sleeps between its boot-indicator
   timer waits; the harness retries the cycle + attach once).
4. After download, reset+detach so the core runs free.
5. Power-cycle once more before measuring: a probe attach leaves the debug power domain up
   (+~200 µA in STOP) until a clean cycle.

If the window proves too short on a given board, flash `lowpower` over the UART bootloader
(`tower flash`, with the board powered) and then run the measurement, or wire NRST.

## The three power-measurement confounders

Every number the power group reports has these handled; if you measure by hand, handle them too:

1. **Debug-domain residual**: any SWD attach adds ~200 µA in STOP until a PPK2-only power
   cycle. Never measure without cycling after a flash.
2. **The supply rail**: measure at the pinned 1.8 V (see PPK2 cabling above); the true floor is
   honest at both rails on this bench, but a knee consumer on another rig was not.
3. **PPK2 CDC desync**: concurrent SWD traffic corrupts the PPK2 byte stream into tens-of-mA
   garbage. Never sample mid-flash; readings above 50 mA are rejected as noise.

Reference floor on this bench (firmware `examples/lowpower.rs`, PPK2 median, no VBUS):
**5.1 µA @ 1.8 V** with the PPK2 the only thing attached (6.1 µA @ 3.0 V); **19.8 µA** with
the J-Link left connected: its SWD debug-domain parasitic adds ~14.7 µA even in STOP. The
power test asserts the median in [1, 50) µA and the mean (floor plus the ~500 ms console
VBUS-poll wake spikes) under 100 µA.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Roster resolve fails at startup | Ports re-enumerated; update `hil.toml` to the names the message lists (or pin `usb_serial`) |
| Roster says "cables or hil.toml slots swapped?" | The `[core]` path holds a Radio Dongle (or vice versa); swap the paths in `hil.toml` |
| Roster says "not a TOWER Core Module/Radio Dongle" | Some other FTDI sits at that path; fix the path, or pin `usb_serial` if it really is the board on a third-party adapter |
| Console read fails: "the device's console frames carry schema tag 0x..." | The board runs firmware built from another checkout than the harness (an older flash, or another tree): every frame header carries the firmware's console schema tag, and it differs from the harness's (the message names the firmware when the frame was its `Hello`); rebuild and reflash from this checkout (the harness's own `just build` does) |
| `tower flash` Write-Memory timeouts, ports renaming mid-run | Concurrent FTDI flashing on one USB bus; the harness flashes sequentially by design; don't parallelize it |
| Power test: median < 1 µA, "probably USB/VBUS-powered" | A VBUS-carrying USB back-feeds VDD (PPK2 reads ~0.3 µA); unplug it, or use a data-only FTDI adapter |
| Power test: "ppk2d … no PPK2 initialised" | Read the reason it gives (unplugged PPK2, missing `ppk2-api`, dangling `.venv` python after a Python upgrade → recreate the venv) |
| Power test: "SWD attach missed the boot window" twice | See "Reflashing a sleeping Core", or flash `lowpower` over UART first, or wire NRST |
| STOP reading ~200 µA too high | Debug domain left up; power-cycle via PPK2 after the last probe-rs contact |
| PPK2 reports tens of mA | CDC desync garbage: re-cycle and re-average; never sample during SWD traffic |
| Bench test hangs on a dead port | Should not happen (resolve fails fast); check the board's USB cable/power |
