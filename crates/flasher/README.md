# jolt — STM32L0 UART programmer

[![CI](https://github.com/hardwario/jolt/actions/workflows/ci.yml/badge.svg)](https://github.com/hardwario/jolt/actions/workflows/ci.yml)

A small, focused Rust CLI that programs **STM32L0** microcontrollers
(STM32L0x1 / L0x2 / L0x3) over a serial port using the STM32 embedded **UART
bootloader** (ST AN3155 protocol). It flashes, erases, reads device info, and
includes a serial monitor.

Any board exposing the STM32L0 bootloader UART works. `jolt` can also enter the
bootloader **automatically** — with no buttons — on boards whose USB-UART bridge
drives the MCU's NRST and BOOT0 pins, which is how it's been tested on the
**HARDWARIO TOWER Radio Dongle** and **TOWER Core Module**. On other boards you
may need to enter the bootloader manually (see [How it works](#how-it-works)).

Flash size is not assumed: a full-chip `erase` adapts to the connected part's
density automatically, so the whole STM32L0 range (16–192 KiB) is supported.

## Build

```sh
cargo build --release
# binary at target/release/jolt
```

macOS builds with a plain `cargo build` (the `serialport` crate pulls only
IOKit deps, no libudev).

## Usage

```
jolt <COMMAND> [OPTIONS]

Commands:
  devices  List connected serial devices
  info     Read bootloader info (chip id, version, commands) — read-only
  flash    Flash a raw firmware .bin file
  erase    Erase the entire flash (adapts to the device's density)
  reset    Reset the device into the application or the bootloader
  monitor  Open the serial device and print incoming data (serial monitor)

Global options:
  -d, --device <DEVICE>   Serial device (default: the only one present, else required)
  -v, --verbose           Verbose output (e.g. per-attempt bootloader-entry errors)
```

The bootloader runs at **115200 baud, 8-E-1** — 115200 is the maximum baud rate
ST specifies for the USART bootloader.

### Examples

```sh
jolt devices
jolt --device /dev/cu.usbserial-112140 info
jolt -d /dev/cu.usbserial-112140 flash firmware.bin
jolt -d /dev/cu.usbserial-112140 flash firmware.bin --no-verify   # skip read-back verify
jolt -d /dev/cu.usbserial-112140 flash firmware.bin --no-erase    # skip erase (see note below)
jolt -d /dev/cu.usbserial-112140 flash firmware.bin --no-run      # leave in bootloader
jolt -d /dev/cu.usbserial-112140 flash firmware.bin --go          # start via Go, not reset
jolt -d /dev/cu.usbserial-112140 erase
jolt -d /dev/cu.usbserial-112140 reset --app                      # or --bootloader
jolt -d /dev/cu.usbserial-112140 monitor                          # 115200 8N1, Ctrl-C to exit
jolt -d /dev/cu.usbserial-112140 monitor -b 9600 --parity even    # 9600 8E1
jolt -d /dev/cu.usbserial-112140 monitor --reset                  # reset into app first, catch boot logs
jolt -d /dev/cu.usbserial-112140 monitor > log.txt                # banner on stderr, device bytes only
```

By default `flash` erases before writing, but only the pages the **image
footprint** spans — not the whole device (a full-chip wipe is the `erase`
command). `--no-erase` skips even that: use it only when the target pages are
already erased. Writing STM32L0 flash that has *not* been erased corrupts the
affected 64-bit words, so `--no-erase` over a non-blank region produces a broken
image (and a verify failure if `--no-verify` isn't also set).

`monitor` is read-only: it streams whatever the device sends to stdout until
you press Ctrl-C. The frame format defaults to **115200 8N1** (the application
console; note this differs from the bootloader's 8-E-1) and is configurable via
`-b/--baudrate`, `--databits` (5–8), `--parity` (none/even/odd), and
`-s/--stopbits` (1–2). Opening the device drives the bridge to the run state so
the application keeps running; `--reset` additionally pulses NRST into the
application so you catch its boot output.

`flash` accepts a **raw `.bin`** only (written at `0x08000000`). Convert ELF/HEX
first, e.g. `arm-none-eabi-objcopy -O binary firmware.elf firmware.bin`.

If exactly one serial device is present, `--device` may be omitted; otherwise pass
it explicitly (use `jolt devices` to find it).

## Library

`jolt` is also a reusable library: the bootloader/flash engine lives in
`src/lib.rs` and is shared with other HARDWARIO tools (e.g. `tower-cli`), which
depend on the crate directly instead of shelling out to the binary.

```rust
use jolt::flash::{self, FlashOptions, Progress};
use jolt::port::Port;

let mut port = Port::open("/dev/ttyUSB0")?;
let firmware = jolt::firmware::load("app.bin".as_ref())?;
let opts = FlashOptions::default();   // erase + verify + run, no `go`

// The engine is UI-free: it reports progress through a callback instead of
// printing, so a TUI or machine-readable frontend stays uncorrupted.
flash::flash(&mut port, &firmware, &opts, &mut |p: Progress| {
    if let Progress::Write { bytes_done, bytes_total } = p {
        eprintln!("{bytes_done}/{bytes_total}");
    }
})?;
// …or discard progress: flash::flash(&mut port, &firmware, &opts, &mut flash::no_progress)
// …or a full-chip wipe:  flash::erase(&mut port, &mut flash::no_progress)
```

The high-level entry points are `flash::flash`, `flash::erase`, and the
`Port::reset_into_app` / `Port::reset_into_bootloader` reset pulses. Every
library entry point returns `jolt::error::Error` (no `anyhow` in the public
API). For callers that already hold an open serial handle, `Port::from_handle`
/ `Port::into_inner` and the `pub` reset-timing constants let you drive the
tuned NRST/BOOT0 sequence without re-implementing it.

## How it works

To enter the bootloader automatically, `jolt` pulses NRST while raising BOOT0,
in raw modem-line terms (`true` = line asserted):

```
(rts=T,dtr=T) → (rts=T,dtr=F) → (rts=F,dtr=T) → (rts=F,dtr=F) → send 0x7F
```

`(T,F)` asserts RESET; switching to `(F,T)` raises BOOT0 while the 1 µF cap on
the TOWER boards lets NRST ramp up slowly, so BOOT0 is high when the chip
latches it and boots system memory. This sequence is tuned for that auto-reset
circuit (FT231X driving NRST/BOOT0 through transistors). **On a board without
it, pull BOOT0 high and reset the MCU yourself**, then run `jolt` — the protocol
itself is hardware-independent.

Once in the bootloader, `jolt` speaks the standard ST protocol (init `0x7F`,
ACK `0x79`). `Get ID` reports the product id (e.g. `0x447` for STM32L0x3);
any recognized STM32L0 id is accepted, and an unknown id only prints a warning.
Erase uses an explicit page list (the STM32L0 bootloader rejects the 0xFFFF
mass-erase code); because the bootloader won't report the flash size, a
full-chip `erase` walks the page list up to the family maximum in 80-page
chunks. When a chunk is NACKed the page list has run past the end of this
part's flash — AN3155 rejects the *whole* chunk if any page is out of range, so
`jolt` then bisects that boundary chunk to find the exact density limit and
erases every valid page below it (rather than dropping the whole chunk and
leaving up to 79 pages un-erased). A NACK to the erase *command byte* itself
(line corruption) is treated as an error, not as the flash boundary.

## Tests

`cargo test` covers the protocol logic with no hardware: command/address/data
checksums and framing, the Get-ID reply parse, the Extended-Erase page list,
write padding, and chunk/address bookkeeping.

## License

Licensed under the [MIT License](LICENSE) — © 2026 HARDWARIO a.s.
