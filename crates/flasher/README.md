# tower-flasher

The flasher library behind `tower flash`, `erase`, `reset`, `chip`, `protect` and `unprotect`:
the STM32 ROM bootloader over UART (AN3155) and, on the Core Module r1.x, over USB DFU (AN3156);
readout protection; the NRST/BOOT0 reset pulses.

[The guide](../../docs/flasher.md): the front end, the library, how it works, USB DFU, readout
protection, tests. Tests: `cargo test -p tower-flasher`.
