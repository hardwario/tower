# jolt: working notes for coding agents

Rust library + `jolt` binary that programs STM32L0 parts (the TOWER Core Module / Radio Dongle's
STM32L083CZ) over the ST UART bootloader (AN3155). The engine lives in the library (`src/lib.rs`:
`bootloader`, `error`, `firmware`, `flash`, `port`, `target`); the binary (`src/main.rs`, `cli`,
`commands`) is a thin front end. User-facing docs are in `README.md`.

## Consumers (pinned by git tag)

`tower-cli` and `tower-hil` depend on the **library** (`default-features = false`, so no clap /
anyhow / indicatif) by git tag (`tag = "vX.Y.Z"`). Keep both consumers on the same tag. A mismatch
is a compile error in the consumer, not a silent interop failure (unlike `tower-protocol`), so it
is an ordinary dependency bump; the TOWER control plane's `/lockstep` reports a divergence
informationally.

- Public API is semver: `Error` is **not** `#[non_exhaustive]`, so adding a variant is a major
  bump. Add detail through existing variants (or a new `#[non_exhaustive]` sub-error such as
  `firmware::ImageError`) unless a major release is intended.
- The library must stay UI-free: report progress through the `Progress` callback, never print.

## Load-bearing invariants

- **Never send readout-protection, write-protection or option-byte commands.** jolt deliberately
  has none; the ROM bootloader must always stay re-enterable (no bricking).
- Writes never leave main flash (192 KiB on the L083): the 6 KiB data EEPROM at `0x0808_0000`
  (radio keys, TX counters) and the option bytes are out of reach, and `erase` wipes main flash
  only.
- The NRST/BOOT0 pulse (`Port::reset_into_bootloader` / `reset_into_app`, RTS = NRST, DTR = BOOT0)
  is tuned for the TOWER auto-reset circuit; its timing constants are `pub` because tower-cli and
  tower-hil reuse them. Don't change them without testing on hardware.
- Ports are opened exclusively (TIOCEXCL + flock). Auto-pick considers USB serial ports only and
  prefers macOS `cu.*` over `tty.*`.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings   # the config consumers build
cargo test
cargo test --no-default-features
```

The tests need no hardware (a scripted fake port drives `flash`/`erase` end to end). Never open a
real serial port or flash a board unless the user asks.

## Releasing

Bump `version` in `Cargo.toml`, commit, push `main`, then tag `vX.Y.Z` and push the tag: the
release workflow gates on the tag matching `Cargo.toml`, the commit being on `main` and CI having
passed, then builds and publishes attested binaries. Afterwards bump the tag in tower-cli **and**
tower-hil in one change-set (`cargo update -p jolt` in each).

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.

## Git workflow

- Developed straight on `main`; no feature branches or PRs unless the user asks.
- Commit and push only when the user requests it.
- MIT © 2026 HARDWARIO a.s.
