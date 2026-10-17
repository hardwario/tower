# tower-boot: boot control for TOWER boards

Boot control for **STM32L0** microcontrollers (STM32L0x1 / L0x2 / L0x3; on TOWER, the
STM32L083CZ of the Core Module and the Radio Dongle) around the STM32 embedded **UART
bootloader** (ST AN3155): flash / erase / verify, read-only chip info, and the NRST/BOOT0
reset pulses that enter the bootloader or restart the application.

It is a **library only** (`crates/boot` in the [hardwario/tower](https://github.com/hardwario/tower)
monorepo). Until the monorepo it was the `jolt` flasher, a library plus a `jolt` binary; the
engine is unchanged, the binary is gone, and its commands live in the `tower` CLI. The `jolt-v*`
tags in this repository are that history.

The engine can enter the bootloader **automatically** (no buttons) on boards whose USB-UART
bridge drives the MCU's NRST and BOOT0 pins, which is how it is used on the **TOWER Radio
Dongle** and the **TOWER Core Module**. On other boards, enter the bootloader manually (see
[How it works](#how-it-works)).

Flash size is not assumed: a full-chip erase adapts to the connected part's density, so the
whole STM32L0 range (16-192 KiB) is supported.

## From `jolt` to `tower`

| Former `jolt` command | Now |
|---|---|
| `jolt devices` | `tower devices` |
| `jolt flash <bin> [--no-erase] [--no-verify] [--no-run] [--go]` | `tower flash <bin>` (same flags, plus `-v`) |
| `jolt erase` | `tower erase` |
| `jolt reset --app` / `--bootloader` | `tower reset` / `tower reset --bootloader` |
| `jolt info` | `tower chip` (chip id + family, bootloader version, supported commands; read-only, the app is restored afterwards) |
| `jolt monitor` | dropped: `tower monitor --hex` dumps the raw bytes |

Every `tower` command takes `-d/--device`, or auto-detects the port when exactly one USB serial
device is present (see `cli/`). Install the CLI from the repo root with
`cargo install --locked --path cli` (or `just install-cli`).

## Library

Consumers in this repository (`cli/`, `hil/`) depend on it by path through the root workspace
(`tower-boot.workspace = true`):

```rust
use tower_boot::flash::{self, FlashOptions, Progress};
use tower_boot::port::Port;

let mut port = Port::open("/dev/ttyUSB0")?;
let firmware = tower_boot::firmware::load("app.bin".as_ref())?;
let opts = FlashOptions::default();   // erase + verify + run, no `go`

// The engine is UI-free: it reports progress through a callback instead of
// printing, so a TUI or machine-readable frontend stays uncorrupted.
flash::flash(&mut port, &firmware, &opts, &mut |p: Progress| {
    if let Progress::Write { bytes_done, bytes_total } = p {
        eprintln!("{bytes_done}/{bytes_total}");
    }
})?;
// ...or discard progress: flash::flash(&mut port, &firmware, &opts, &mut flash::no_progress)
// ...or a full-chip wipe:  flash::erase(&mut port, &mut flash::no_progress)
// ...or a read-only query: tower_boot::chip::info(&mut port, &mut flash::no_progress)
```

The high-level entry points:

- `flash::flash`: enter the bootloader, `Get ID`, erase the image footprint, write, verify,
  start the application (each step optional through `FlashOptions`);
- `flash::erase`: full-chip erase of main flash, then back into the application;
- `chip::info`: read-only, returns a `ChipInfo` (`id`, `family`, `bootloader_version`,
  `commands`, plus `is_known()` / `bootloader_version_parts()`) and restores the application on
  every path; this is what `tower chip` prints;
- `Port::reset_into_app` / `Port::reset_into_bootloader`: the reset pulses.

Every library entry point returns `tower_boot::error::Error` (no `anyhow` in the public API), and
nothing in the library prints. For callers that already hold an open serial handle,
`Port::from_handle` / `Port::into_inner` and the `pub` reset-timing constants let you drive the
tuned NRST/BOOT0 sequence without re-implementing it (the `tower` CLI and the HIL harness reuse
them for the console reset).

Notes that carry over from the CLI days:

- The bootloader runs at **115200 baud, 8-E-1** (`target::BAUD`); 115200 is the maximum baud
  rate ST specifies for the USART bootloader.
- `FlashOptions::erase` erases only the pages the **image footprint** spans, not the whole
  device (a full-chip wipe is `flash::erase`). Writing STM32L0 flash that has *not* been erased
  corrupts the affected 64-bit words, so skip the erase only when the target pages are already
  blank.
- Erase wipes **main flash only**; the data EEPROM (6 KiB on the L083: radio keys, TX counters)
  survives. Use the firmware's EEPROM wipe (`/system eeprom wipe confirm` in the TOWER shell) to
  clear it.
- `firmware::load` accepts a **raw `.bin`** only (written at `0x08000000`). Convert ELF/HEX
  first, e.g. `arm-none-eabi-objcopy -O binary firmware.elf firmware.bin` (or
  `cargo objcopy --release -- -O binary out.bin`, which `just fw build` does). The file is
  checked by content too: ELF/Intel HEX are rejected whatever their extension, and the Cortex-M
  vector table must be sane (initial SP in SRAM, Thumb reset vector inside the image).

## How it works

To enter the bootloader automatically, the engine pulses NRST while raising BOOT0, in raw
modem-line terms (`true` = line asserted):

```
(rts=T,dtr=T) → (rts=T,dtr=F) → (rts=F,dtr=T) → (rts=F,dtr=F) → send 0x7F
```

`(T,F)` asserts RESET; switching to `(F,T)` raises BOOT0 while the 1 µF cap on the TOWER boards
lets NRST ramp up slowly, so BOOT0 is high when the chip latches it and boots system memory. This
sequence is tuned for that auto-reset circuit (FT231X driving NRST/BOOT0 through transistors).
**On a board without it, pull BOOT0 high and reset the MCU yourself**; the protocol itself is
hardware-independent.

Once in the bootloader, the engine speaks the standard ST protocol (init `0x7F`, ACK `0x79`).
`Get ID` reports the product id (e.g. `0x447` for STM32L0x3); any recognized STM32L0 id is
accepted, and an unknown id is only reported (a front end may warn). Erase uses an explicit page
list (the STM32L0 bootloader rejects the 0xFFFF mass-erase code); because the bootloader won't
report the flash size, a full-chip erase walks the page list up to the family maximum in 80-page
chunks. When a chunk is NACKed the page list has run past the end of this part's flash. AN3155
rejects the *whole* chunk if any page is out of range, so the engine then bisects that boundary
chunk to find the exact density limit and erases every valid page below it (rather than
dropping the whole chunk and leaving up to 79 pages un-erased). A NACK to the erase *command
byte* itself (line corruption) is treated as an error, not as the flash boundary.

## Tests

From the repo root, `cargo test -p tower-boot` covers the protocol logic with no hardware:
command/address/data checksums and framing, the Get-ID reply parse, the Extended-Erase page
list, write padding, chunk/address bookkeeping, and a scripted fake serial port that drives
`flash`, `erase` and `chip::info` end to end (including the reset-into-app restore paths).
Linux builds need `libudev-dev` + `pkg-config` (the `serialport` dependency).

## License

MIT © 2026 HARDWARIO a.s. (see the repository's root `LICENSE`).
