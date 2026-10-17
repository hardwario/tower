# tower-boot: working notes for coding agents

Library that drives the STM32 ROM UART bootloader (AN3155) of STM32L0 parts (the TOWER Core
Module / Radio Dongle's STM32L083CZ): flash / erase / verify, read-only chip info, and the
NRST/BOOT0 reset pulses. `crates/boot` in the hardwario/tower monorepo (read the root
`AGENTS.md` first). Modules (`src/lib.rs`): `bootloader` (the AN3155 protocol engine), `chip`
(`chip::info`, read-only `ChipInfo`), `error`, `firmware` (raw image load + validation), `flash`
(connect / flash / erase orchestration), `port` (the serial port + reset pulses), `target`
(STM32L0 constants). User-facing docs are in `README.md`.

It was the `jolt` flasher (library + `jolt` binary) until the monorepo. The binary is gone: its
commands live in the `tower` CLI (`tower flash` / `erase` / `reset` / `chip` / `devices`; the
raw serial `monitor` was dropped in favour of `tower monitor --hex`). The `jolt-v*` tags are
history.

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

- **Never send readout-protection, write-protection or option-byte commands.** The crate
  deliberately has none; the ROM bootloader must always stay re-enterable (no bricking).
- Writes never leave main flash (192 KiB on the L083): the 6 KiB data EEPROM at `0x0808_0000`
  (radio keys, TX counters) and the option bytes are out of reach, and `erase` wipes main flash
  only.
- Read-only operations restore the application on every path (`chip::info`, and `flash::connect`
  when bootloader entry fails), so a query never leaves a board parked in the bootloader.
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

The tests need no hardware (a scripted fake port drives `flash`, `erase` and `chip::info` end to
end). Never open a real serial port or flash a board unless the user asks.

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.

## Git workflow

- Part of the hardwario/tower monorepo, developed straight on `main`; no feature branches or PRs
  unless the user asks.
- Commit and push only when the user requests it.
- MIT © 2026 HARDWARIO a.s.
