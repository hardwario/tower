# tower-flasher: the TOWER flasher

The flasher of **STM32L0** microcontrollers (STM32L0x1 / L0x2 / L0x3; on TOWER, the
STM32L083CZ of the Core Module and the Radio Dongle) around the STM32 embedded **UART
bootloader** (ST AN3155): flash / erase / verify, read-only chip info, the opt-in **readout
protection** (RDP level 1) of production units, and the NRST/BOOT0 reset pulses that enter the
bootloader or restart the application.

It is a **library only** (`crates/flasher` in the [hardwario/tower](https://github.com/hardwario/tower)
monorepo); its front end is the `tower` CLI.

The engine can enter the bootloader **automatically** (no buttons) on boards whose USB-UART
bridge drives the MCU's NRST and BOOT0 pins, which is how it is used on the **TOWER Radio
Dongle** and the **TOWER Core Module**. On other boards, enter the bootloader manually (see
[How it works](#how-it-works)).

Flash size is not assumed: a full-chip erase adapts to the connected part's density, so the
whole STM32L0 range (16-192 KiB) is supported.

## Front end

The `tower` CLI drives the engine: `tower flash <bin> [--no-erase] [--no-verify] [--no-run]
[--go]`, `tower erase`, `tower reset [--bootloader]`, `tower chip` (chip id + family, readout
protection, bootloader version, supported commands; read-only, the app is restored afterwards)
and `tower protect` / `tower unprotect` (RDP level 1 on and off, see
[Readout protection](#readout-protection)); `tower monitor --hex` dumps the raw serial bytes.

Every serial-port `tower` command takes `-d/--device`, or auto-detects the port when exactly one
USB serial device is present (see `cli/`). Install the CLI from the repo root with
`cargo install --locked --path cli` (or `just install-cli`).

## Library

Consumers in this repository (`cli/`, `hil/`) depend on it by path through the root workspace
(`tower-flasher.workspace = true`):

```rust
use tower_flasher::flash::{self, FlashOptions, Progress};
use tower_flasher::port::Port;

let mut port = Port::open("/dev/ttyUSB0")?;
let firmware = tower_flasher::firmware::load("app.bin".as_ref())?;
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
// ...or a read-only query: tower_flasher::chip::info(&mut port, &mut flash::no_progress)
// ...or the production lock:  tower_flasher::rdp::protect(&mut port, &mut flash::no_progress)
```

The high-level entry points:

- `flash::flash`: enter the bootloader, `Get ID`, erase the image footprint, write, verify,
  start the application (each step optional through `FlashOptions`);
- `flash::erase`: full-chip erase of main flash, then back into the application;
- `chip::info`: read-only, returns a `ChipInfo` (`id`, `family`, `bootloader_version`,
  `commands`, `readout_protected`, plus `is_known()` / `bootloader_version_parts()`) and restores
  the application on every path; this is what `tower chip` prints;
- `rdp::protect` / `rdp::unprotect`: readout protection level 1 on / off, each verified by a
  read-back in a new bootloader session; they return an `rdp::Change` (`id`, `was_protected`,
  `readout_protected`, `changed()`). `unprotect` mass-erases the flash and the data EEPROM;
- `Port::reset_into_app` / `Port::reset_into_bootloader`: the reset pulses.

Every library entry point returns `tower_flasher::error::Error` (no `anyhow` in the public API), and
nothing in the library prints. For callers that already hold an open serial handle,
`Port::from_handle` / `Port::into_inner` and the `pub` reset-timing constants let you drive the
tuned NRST/BOOT0 sequence without re-implementing it (the `tower` CLI and the HIL harness reuse
them for the console reset).

Operating notes:

- The bootloader runs at **115200 baud, 8-E-1** (`target::BAUD`); 115200 is the maximum baud
  rate ST specifies for the USART bootloader.
- `FlashOptions::erase` erases only the pages the **image footprint** spans, not the whole
  device (a full-chip wipe is `flash::erase`). Writing STM32L0 flash that has *not* been erased
  corrupts the affected 64-bit words, so skip the erase only when the target pages are already
  blank.
- Erase wipes **main flash only**; the data EEPROM (6 KiB on the L083: radio keys, TX counters)
  survives. Use the firmware's EEPROM wipe (`system eeprom wipe confirm` in the TOWER shell) to
  clear it. (The one library path that clears it is `rdp::unprotect` on a protected chip.)
- Writes are bounded to main flash in the protocol engine itself (`Bootloader::write_block`
  refuses any other address), so the option bytes and the data EEPROM are out of reach.
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

## Readout protection

TOWER dev boards stay **open** (RDP level 0). For production units, readout protection level 1
is an explicit, opt-in provisioning step: `rdp::protect` (`tower protect`). The full rationale and
the ST citations are in the `rdp` module docs (`src/rdp.rs`); in short:

- **Level 1** (RM0367 3.4.1): while the bootloader, a RAM boot or a debugger (SWD) is connected,
  the flash and the data EEPROM can be neither read nor written; booted from flash, the
  application runs normally. The bootloader then serves only Get, Get Version, Get ID and Readout
  Unprotect (AN3155): Read Memory, Write Memory, Go and Erase are NACKed, so `flash::flash` and
  `flash::erase` fail fast with `Error::ReadoutProtected` (detected by a probe right after
  `Get ID`, before anything is sent).
- **Undoing it** (`rdp::unprotect`, `tower unprotect`) goes from level 1 to level 0, which
  **mass-erases the flash AND the data EEPROM** (RM0367 3.4.1): firmware, radio keys, pairing,
  settings and TX counters are gone, and the node must be re-flashed and re-paired. A chip that
  is not protected is left alone.
- **Level 2 is never reachable from here.** It is permanent (no SWD, no bootloader, no way back;
  RM0367 3.4.1, AN2606 4.1). The bootloader's Readout Protect only selects level 1, the crate has
  no option-byte write, and `Bootloader::write_block` cannot address the option bytes.
- **The probe** (behind `ChipInfo::readout_protected`): a Read Memory of 4 bytes at
  `0x0800_0000`. Data means open; a NACK to the command byte means protected, but only when a
  `Get ID` right after it is answered (the link is fine) and a second Read Memory is refused too.
  Get Version's two "option bytes" are fixed at 0x00 on these bootloaders, so they cannot tell.
- **Power cycle:** after level 1 is set through the bootloader, the STM32L0 runs user code only
  after a power-down (RM0367 3.4.1), so power-cycle a freshly protected board once. With a
  debugger attached since the last power-on, a system reset does not even load the new option
  bytes (RM0367 3.4); `protect` then reports `Error::ReadoutUnchanged`, and a power cycle plus
  `tower chip` shows the real state.

Both entry points: enter the bootloader, `Get ID`, probe; send the command only if the state
has to change (`82 7D` or `92 6D`, first ACK, then the second ACK once the option bytes are
written, after the mass erase for unprotect); the device resets itself; re-enter the bootloader
the normal way, probe again, reset into the application. The two opcodes are sent nowhere else,
and the tests pin that.

## Tests

From the repo root, `cargo test -p tower-flasher` covers the protocol logic with no hardware:
command/address/data checksums and framing, the Get-ID reply parse, the Extended-Erase page
list, write padding, chunk/address bookkeeping, the main-flash bound of Write Memory, the
readout-protection handshakes and probe, and a scripted fake serial port that drives `flash`,
`erase`, `chip::info`, `rdp::protect` and `rdp::unprotect` end to end (including the
reset-into-app restore paths, the fail-fast on a protected chip, and a check that no other entry
point sends Readout Protect / Unprotect).
Linux builds need `libudev-dev` + `pkg-config` (the `serialport` dependency).

## License

MIT © 2026 HARDWARIO a.s. (see the repository's root `LICENSE`).
