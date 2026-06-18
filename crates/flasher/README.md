# towerf — TOWER Flasher

A small, focused Rust CLI that programs an **STM32L083CZ** over a serial port
using the STM32 embedded **UART bootloader** (ST AN3155 protocol). It was built
for the **HARDWARIO TOWER Radio Dongle**, whose USB-UART bridge drives the MCU's
NRST and BOOT0 pins (with a 1 µF cap on NRST), so the chip can be flashed over
USB with no buttons.

## Build

```sh
cargo build --release
# binary at target/release/towerf
```

macOS builds with a plain `cargo build` (the `serialport` crate pulls only
IOKit deps, no libudev).

## Usage

```
towerf <COMMAND> [OPTIONS]

Commands:
  list     List all serial ports
  info     Read bootloader info (chip id, version, commands) — read-only
  flash    Flash a raw firmware .bin file
  erase    Erase the entire flash
  reset    Reset the device into the application or the bootloader

Global options:
  -p, --port <PATH>   Serial port (default: the only port present, else required)
  -v, --verbose       Verbose output (e.g. per-attempt bootloader-entry errors)
```

The bootloader runs at **115200 baud, 8-E-1** — 115200 is the maximum baud rate
ST specifies for the USART bootloader.

### Examples

```sh
towerf list
towerf --port /dev/cu.usbserial-112140 info
towerf -p /dev/cu.usbserial-112140 flash firmware.bin
towerf -p /dev/cu.usbserial-112140 flash firmware.bin --no-verify   # skip read-back verify
towerf -p /dev/cu.usbserial-112140 flash firmware.bin --no-run      # leave in bootloader
towerf -p /dev/cu.usbserial-112140 flash firmware.bin --go          # start via Go, not reset
towerf -p /dev/cu.usbserial-112140 erase
towerf -p /dev/cu.usbserial-112140 reset --app                      # or --bootloader
```

`flash` accepts a **raw `.bin`** only (written at `0x08000000`). Convert ELF/HEX
first, e.g. `arm-none-eabi-objcopy -O binary firmware.elf firmware.bin`.

If exactly one serial port is present, `--port` may be omitted; otherwise pass
it explicitly (use `towerf list` to find it).

## How it works

To enter the bootloader, `towerf` pulses NRST while raising BOOT0, in raw
modem-line terms (`true` = line asserted):

```
(rts=T,dtr=T) → (rts=T,dtr=F) → (rts=F,dtr=T) → (rts=F,dtr=F) → send 0x7F
```

`(T,F)` asserts RESET; switching to `(F,T)` raises BOOT0 while the 1 µF cap lets
NRST ramp up slowly, so BOOT0 is high when the chip latches it and boots system
memory. It then speaks the standard ST protocol (init `0x7F`, ACK `0x79`);
`Get ID` returns **0x447** for the STM32L0x3. Erase uses an explicit page list
(the STM32L0 bootloader rejects the 0xFFFF mass-erase code).

## Tests

`cargo test` covers the protocol logic with no hardware: command/address/data
checksums and framing, the Get-ID reply parse, the Extended-Erase page list,
write padding, and chunk/address bookkeeping.
