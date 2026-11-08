#!/usr/bin/env python3
"""RAM-budget guard for the TOWER firmware bins (STM32L083CZ, 20 KB SRAM).

flip-link places the statics (`.data`/`.bss`/`.uninit`) at the TOP of RAM and the stack
BELOW them, so a stack overflow faults below the RAM base instead of silently corrupting
`.bss`. That makes the **stack budget** exactly `(lowest static section address) - RAM_BASE`,
readable straight from the ELF, with no llvm-tools / cargo-binutils needed (this parses the ELF
section headers directly so it runs in CI with only python3).

Every product bin must leave at least `STACK_FLOOR` of stack. The floor is calibrated to the
measured stack high-water mark (SWD stack-painting on real hardware, 2026-07-11):
  * radio_push_button  ~6.8 KB peak, stable under KV-compaction churn  (budget 9.4 KB)
  * radio_dongle_gateway console-idle ~3.3 KB; its deep registry/mgmt paths were not
    bench-driveable standalone, est. ~7.5 KB worst-case                 (budget 8.6 KB)
An 8 KB floor keeps ~1 KB over the measured/estimated peaks. If a bin legitimately needs
more statics, raise STACK_FLOOR *consciously* and re-measure the high-water mark on hardware
(see docs/gateway.md "RAM budget"); do not just nudge the number to make CI green.

A bin without the radio stack can carry its own floor in BIN_FLOORS, set from ITS measured
high-water mark with the same margin discipline:
  * led_strip  2.8 KB peak (`system stack print` after effects, every shell command and
    repeated `strip save` EEPROM writes, 2026-11-08); floor 6 KB, over 2x the peak, leaves room
    for a KV compaction. Its statics are the strip's 4.6 KB wire buffer.

Usage: check_ram_budget.py <bin-name> [<bin-name> ...]
       (ELFs are read from target/thumbv6m-none-eabi/release/<bin-name>)
"""

import struct
import sys
from pathlib import Path

RAM_BASE = 0x2000_0000
RAM_SIZE = 20 * 1024
STACK_FLOOR = 8 * 1024  # bytes; every bin must leave at least this much stack
# Measured per-bin floors for bins without the radio stack (see the docstring).
BIN_FLOORS = {"led_strip": 6 * 1024}
TARGET_DIR = Path("target/thumbv6m-none-eabi/release")


def stack_top(elf: bytes) -> int:
    """Lowest address of any allocated RAM section = flip-link's stack ceiling."""
    if elf[:4] != b"\x7fELF" or elf[4] != 1:
        raise SystemExit("not a 32-bit ELF")
    e_shoff = struct.unpack_from("<I", elf, 0x20)[0]
    e_shentsize = struct.unpack_from("<H", elf, 0x2E)[0]
    e_shnum = struct.unpack_from("<H", elf, 0x30)[0]
    tops = []
    for i in range(e_shnum):
        # section header: name, type, flags, addr, offset, size, ...
        _, _, _, addr, _, size = struct.unpack_from("<IIIIII", elf, e_shoff + i * e_shentsize)
        if size > 0 and RAM_BASE <= addr < RAM_BASE + RAM_SIZE:
            tops.append(addr)
    if not tops:
        raise SystemExit("no RAM sections found")
    return min(tops)


def main(bins) -> None:
    if not bins:
        raise SystemExit("usage: check_ram_budget.py <bin-name> [<bin-name> ...]")
    print(f"RAM budget: {RAM_SIZE // 1024} KB SRAM, stack floor {STACK_FLOOR // 1024} KB (flip-link layout)\n")
    print(f"  {'bin':<26} {'statics':>8} {'stack':>8}   verdict")
    failed = False
    for name in bins:
        elf = TARGET_DIR / name
        if not elf.exists():
            raise SystemExit(f"{elf} not found; build first (cargo build --release --bins)")
        top = stack_top(elf.read_bytes())
        stack = top - RAM_BASE
        statics = RAM_SIZE - stack
        floor = BIN_FLOORS.get(name, STACK_FLOOR)
        ok = stack >= floor
        failed |= not ok
        if not ok:
            note = f"FAIL (< {floor} floor)"
        elif floor != STACK_FLOOR:
            note = f"OK (own floor {floor})"
        else:
            note = "OK"
        print(f"  {name:<26} {statics:>8} {stack:>8}   {note}")
    if failed:
        print(
            "\nRAM budget exceeded: a bin's statics grew until the stack fell below the floor.\n"
            "Trim resident state (async task futures dominate; see docs/gateway.md), or, if the\n"
            "growth is justified, raise STACK_FLOOR after re-measuring the stack high-water mark."
        )
        sys.exit(1)
    print("\nRAM budget OK")


if __name__ == "__main__":
    main(sys.argv[1:])
