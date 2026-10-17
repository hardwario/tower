# tower-boot: working notes for coding agents

Library that drives the STM32 ROM UART bootloader (AN3155) of STM32L0 parts (the TOWER Core
Module / Radio Dongle's STM32L083CZ): flash / erase / verify, read-only chip info, the opt-in
readout protection (RDP level 1) of production units, and the NRST/BOOT0 reset pulses.
`crates/boot` in the hardwario/tower monorepo (read the root `AGENTS.md` first). Modules
(`src/lib.rs`): `bootloader` (the AN3155 protocol engine), `chip` (`chip::info`, read-only
`ChipInfo` incl. `readout_protected`), `error`, `firmware` (raw image load + validation), `flash`
(connect / flash / erase orchestration), `port` (the serial port + reset pulses), `rdp`
(`rdp::protect` / `rdp::unprotect` and the protection probe; its module docs hold the RM0367 /
AN3155 / AN2606 facts with citations), `target` (STM32L0 constants). User-facing docs are in
`README.md`.

Its front end is the `tower` CLI (`tower flash` / `erase` / `reset` / `chip` / `protect` /
`unprotect` / `devices`). It was the standalone `jolt` flasher before the monorepo; the `jolt-v*`
tags are its history.

## Consumers

`tower-cli` (`cli/`) and `tower-hil` (`hil/`) depend on it **by path** through the root workspace
(`tower-boot.workspace = true`). It is an internal crate: no releases, no tags of its own, and an
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
- Writes never leave main flash (192 KiB on the L083), enforced in `Bootloader::write_block`: the
  6 KiB data EEPROM at `0x0808_0000` (radio keys, TX counters) and the option bytes are out of
  reach, and `erase` wipes main flash only. The one thing that wipes the data EEPROM is
  `rdp::unprotect`'s mass erase, and it does nothing on a chip that is not protected.
- Read-only operations restore the application on every path (`chip::info`, and `flash::connect`
  when bootloader entry fails), so a query never leaves a board parked in the bootloader. So do
  `rdp::protect` / `unprotect` (on errors too, best effort) and a `flash` / `erase` refused by the
  probe. After RDP level 1 is set through the bootloader the STM32L0 runs user code only after a
  power-down (RM0367 3.4.1): that power cycle is the user's, the library cannot do it.
- The NRST/BOOT0 pulse (`Port::reset_into_bootloader` / `reset_into_app`, RTS = NRST, DTR = BOOT0)
  is tuned for the TOWER auto-reset circuit; its timing constants are `pub` because the `tower`
  CLI and the HIL harness reuse them for their console resets. Don't change them without testing
  on hardware.
- Ports are opened exclusively (TIOCEXCL + flock).

## Checks

From the repo root (`just check` runs every gate):

```sh
cargo fmt --all -- --check
cargo clippy -p tower-boot --all-targets -- -D warnings
cargo test -p tower-boot
```

The tests need no hardware (a scripted fake port drives `flash`, `erase`, `chip::info`,
`rdp::protect` and `rdp::unprotect` end to end). Never open a real serial port, flash a board or
change its protection unless the user asks.

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.

## Git workflow

- Part of the hardwario/tower monorepo, developed straight on `main`; no feature branches or PRs
  unless the user asks.
- Commit and push only when the user requests it.
- MIT © 2026 HARDWARIO a.s.
