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
  list     List all serial ports
  info     Read bootloader info (chip id, version, commands) — read-only
  flash    Flash a raw firmware .bin file
  erase    Erase the entire flash (adapts to the device's density)
  reset    Reset the device into the application or the bootloader
  monitor  Open the serial port and print incoming data (serial monitor)

Global options:
  -p, --port <PATH>   Serial port (default: the only port present, else required)
  -v, --verbose       Verbose output (e.g. per-attempt bootloader-entry errors)
```

The bootloader runs at **115200 baud, 8-E-1** — 115200 is the maximum baud rate
ST specifies for the USART bootloader.

### Examples

```sh
jolt list
jolt --port /dev/cu.usbserial-112140 info
jolt -p /dev/cu.usbserial-112140 flash firmware.bin
jolt -p /dev/cu.usbserial-112140 flash firmware.bin --no-verify   # skip read-back verify
jolt -p /dev/cu.usbserial-112140 flash firmware.bin --no-run      # leave in bootloader
jolt -p /dev/cu.usbserial-112140 flash firmware.bin --go          # start via Go, not reset
jolt -p /dev/cu.usbserial-112140 erase
jolt -p /dev/cu.usbserial-112140 reset --app                      # or --bootloader
jolt -p /dev/cu.usbserial-112140 monitor                          # 115200 8N1, Ctrl-C to exit
jolt -p /dev/cu.usbserial-112140 monitor -b 9600 --parity even    # 9600 8E1
jolt -p /dev/cu.usbserial-112140 monitor --reset                  # reset into app first, catch boot logs
jolt -p /dev/cu.usbserial-112140 monitor > log.txt                # banner on stderr, device bytes only
```

`monitor` is read-only: it streams whatever the device sends to stdout until
you press Ctrl-C. The frame format defaults to **115200 8N1** (the application
console; note this differs from the bootloader's 8-E-1) and is configurable via
`-b/--baudrate`, `-d/--databits` (5–8), `--parity` (none/even/odd), and
`-s/--stopbits` (1–2). Opening the port drives the bridge to the run state so
the application keeps running; `--reset` additionally pulses NRST into the
application so you catch its boot output.

`flash` accepts a **raw `.bin`** only (written at `0x08000000`). Convert ELF/HEX
first, e.g. `arm-none-eabi-objcopy -O binary firmware.elf firmware.bin`.

If exactly one serial port is present, `--port` may be omitted; otherwise pass
it explicitly (use `jolt list` to find it).

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
full-chip `erase` walks the page list up to the family maximum and stops as soon
as the device NACKs an out-of-range page — i.e. once the part's flash is wiped.

## Tests

`cargo test` covers the protocol logic with no hardware: command/address/data
checksums and framing, the Get-ID reply parse, the Extended-Erase page list,
write padding, and chunk/address bookkeeping.

## License

Licensed under the [MIT License](LICENSE) — © 2026 HARDWARIO a.s.
