# tower-flasher: working notes for coding agents

Library that drives the STM32 ROM bootloader of STM32L0 parts (the TOWER Core Module / Radio
Dongle's STM32L083CZ): over UART (AN3155) flash / erase / verify, read-only chip info, the opt-in
readout protection (RDP level 1) of production units, and the NRST/BOOT0 reset pulses; over USB
DFU (AN3156, the `dfu` module, for the Core Module r1.x that has no USB-UART bridge) flash /
erase / verify / chip info.
`crates/flasher` in the hardwario/tower monorepo (read the root `AGENTS.md` first). Modules
(`src/lib.rs`): `bootloader` (the AN3155 protocol engine), `chip` (`chip::info`, read-only
`ChipInfo` incl. `readout_protected`), `error`, `firmware` (raw image load + validation), `flash`
(connect / flash / erase orchestration), `port` (the serial port + reset pulses), `rdp`
(`rdp::protect` / `rdp::unprotect` and the protection probe; its module docs hold the RM0367 /
AN3155 / AN2606 facts with citations), `target` (STM32L0 constants), `dfu` (the DfuSe engine and
its memory-layout parser; `dfu/usb.rs` the nusb transport, `dfu/fake.rs` the simulated bootloader
of its tests; the module docs hold the AN3156 facts). User-facing docs are in `README.md`.

Its front end is the `tower` CLI (`tower flash` / `erase` / `reset` / `chip` / `protect` /
`unprotect` / `devices`).

## Consumers

`tower-cli` (`cli/`) and `tower-hil` (`hil/`) depend on it **by path** through the root workspace
(`tower-flasher.workspace = true`). It is an internal crate: no releases, no tags of its own, and an
API change is made together with its callers in the same commit (the build fails until they
are updated).

- The library must stay UI-free: report progress through the `Progress` callback, never print.
  Front-end concerns (progress bars, port auto-pick, wording of warnings) belong to the `tower`
  CLI (`cli/src/main.rs`, `cli/src/port.rs`'s `pick_port`).
- Every entry point returns the typed `error::Error`; no `anyhow` in the library.

## Load-bearing invariants

- **Readout protection: level 1 only, only through `rdp::protect` / `rdp::unprotect`; never an
  option-byte write, never level 2.** Readout Protect (0x82) and Readout Unprotect (0x92) are
  sent by two crate-private `Bootloader` methods whose only callers are `rdp::protect` /
  `rdp::unprotect` (a test scans every `src/*.rs` and pins that, and another runs `flash`,
  `erase` and `chip::info` and checks neither command frame reaches the wire). Write Protect /
  Write Unprotect (0x63 / 0x73) are not implemented, and `Bootloader::write_block` refuses any
  address outside main flash, so the option bytes (the RDP byte; 0xCC there is level 2) cannot be
  written even through the public API. Why: level 1 is reversible (Readout Unprotect, at the price
  of a mass erase of the flash AND the data EEPROM), so a protected board stays re-enterable and
  re-flashable; level 2 permanently disables SWD, the bootloader and option-byte changes (RM0367
  3.4.1, AN2606 4.1), which would brick a board for any re-flash or failure analysis. Protection
  is opt-in: dev boards stay at level 0 and nothing else in the crate changes the level.
- Never guess the protection state. The probe (`rdp::probe`: a Read Memory of 4 bytes at the
  flash base; a command-stage NACK counts as protected only when a `Get ID` right after it is
  served, twice) and why it beats Get Version's option bytes (fixed 0x00) are documented in
  `src/rdp.rs`; a probe without a clean answer is an error, never a default. `flash` / `erase`
  probe right after `Get ID` and fail with `Error::ReadoutProtected` before anything is sent.
- Writes and erases never leave main flash (192 KiB on the L083), enforced in
  `Bootloader::write_block` (addresses) and `Bootloader::extended_erase_pages` (pages < 1536):
  the 6 KiB data EEPROM at `0x0808_0000` (radio keys, TX counters) and the option bytes are out
  of reach, and `erase` wipes main flash only. The one thing that wipes the data EEPROM is
  `rdp::unprotect`'s mass erase, and it does nothing on a chip that is not protected.
- `flash` writes the first 256-byte block (the vector table) last, after the rest is written
  and verified: an interrupted flash leaves it blank (erased), so a truncated image never boots.
  `flash` / `erase` refuse an unknown product id (`Error::UnknownChip`) before anything is
  erased; only `chip::info` reports one.
- Read-only operations restore the application on every path (`chip::info`, and `flash::connect`
  when bootloader entry fails), so a query never leaves a board parked in the bootloader. So do
  `rdp::protect` / `unprotect` (on errors too, best effort) and a `flash` / `erase` refused by the
  probe. After RDP level 1 is set through the bootloader the STM32L0 runs user code only after a
  power-down (RM0367 3.4.1): that power cycle is the user's, the library cannot do it.
- The NRST/BOOT0 pulse (`Port::reset_into_bootloader` / `reset_into_app`, RTS = NRST, DTR = BOOT0)
  is tuned for the TOWER auto-reset circuit; its timing constants are `pub` because the `tower`
  CLI and the HIL harness reuse them for their console resets. Don't change them without testing
  on hardware. `flash::connect` doubles the BOOT0 hold on each retry (50 ms up to
  `BOOT_RELEASE_HOLD_MAX`, 400 ms); the line sequence is pinned by a test.
- Ports are opened exclusively (TIOCEXCL + flock).
- **USB DFU keeps the same rules** (`src/dfu.rs`): only the alternate setting whose layout is the
  main flash at `0x0800_0000` is selected (`Layout::check_main_flash`), and `Dfu::erase_page` /
  `Dfu::write` / `Dfu::read` refuse anything outside it; only Set Address Pointer and the page
  Erase with an address are ever built (no mass erase, no Read Unprotect `0x92`; a test scans the
  source and the fake panics on any other command); the first transfer block (vector table) is
  written last; the protection probe (`Dfu::readout_protected`: `errVENDOR` on a read of the flash
  base, AN3156 4.1) runs before anything is modified and only `errVENDOR` counts as protected;
  readout protection is never changed over DFU. Unlike the UART path the board is left in the
  bootloader on failures (no reset line; restarting it costs a manual re-entry): only a successful
  `dfu::flash` with `run` leaves DFU. `Dfu::leave` polls until the device is off the bus and
  nudges the STM32L0's bootloader with a refused `DFU_ABORT` when it lingers in `dfuMANIFEST`
  (seen on hardware); don't drop that. Verified on a rev 1.3 Core Module (2026-10-19): layout
  `1536 x 128 B`, transfer 2048 B, bootloader v2.2.

## Checks

From the repo root (`just check` runs every gate):

```sh
cargo fmt --all -- --check
cargo clippy -p tower-flasher --all-targets -- -D warnings
cargo test -p tower-flasher
```

The tests need no hardware (a scripted fake port drives `flash`, `erase`, `chip::info`,
`rdp::protect` and `rdp::unprotect` end to end; `src/fake_stm32.rs` is a stateful simulated
STM32L0 with fault injection for the wire-exact flash tests; `src/dfu/fake.rs` simulates the
DfuSe bootloader for `dfu`). Never open a real serial port or USB device, flash a board or
change its protection unless the user asks.

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.

## Git workflow

- Part of the hardwario/tower monorepo, developed straight on `main`; no feature branches or PRs
  unless the user asks.
- Commit and push only when the user requests it.
- MIT © 2026 HARDWARIO a.s.
