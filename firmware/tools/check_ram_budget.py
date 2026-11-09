#!/usr/bin/env python3
"""RAM-budget guard for the TOWER firmware bins (STM32L083CZ, 20 KB SRAM).

flip-link places the statics (`.data`/`.bss`/`.uninit`) at the TOP of RAM and the stack
BELOW them, so a stack overflow faults below the RAM base instead of silently corrupting
`.bss`. That makes the **stack budget** exactly `(lowest static section address) - RAM_BASE`,
readable straight from the ELF, with no llvm-tools / cargo-binutils needed (this parses the ELF
section headers directly so it runs in CI with only python3).

Every product bin must leave at least `STACK_FLOOR` of stack. The floor is calibrated to the
measured stack high-water mark (SWD stack-painting on real hardware, 2026-07-11):
  * radio_button  ~6.8 KB peak, stable under KV-compaction churn  (budget 9.4 KB)
  * radio_gateway console-idle ~3.3 KB; its deep registry/mgmt paths were not
    bench-driveable standalone, est. ~7.5 KB worst-case                 (budget 8.6 KB)
An 8 KB floor keeps ~1 KB over the measured/estimated peaks. If a bin legitimately needs
more statics, raise STACK_FLOOR *consciously* and re-measure the high-water mark on hardware
(see docs/firmware/gateway.md "RAM budget"); do not just nudge the number to make CI green.

It also reports each bin's free flash and warns (without failing) when an image comes within
`FLASH_WARN` of the 192 KB: code added to the SDK or the taf runtime lands in every app, and the
largest ones (radio_thermostat on the r2 even at opt-level "z", with its LCD screen; radio_control
on the r2) are the first to overflow (TASKS.md, T18 and D14; docs/firmware/taf.md, "RAM").

Usage: check_ram_budget.py <bin-name>[@<profile>] [...]
       (ELFs are read from target/thumbv6m-none-eabi/<profile>/<bin-name>, profile `release`
       unless named: `radio_thermostat@release-z`, the apps the justfile builds at "z")
"""

import struct
import sys
from pathlib import Path

RAM_BASE = 0x2000_0000
RAM_SIZE = 20 * 1024
STACK_FLOOR = 8 * 1024  # bytes; every bin must leave at least this much stack
FLASH_BASE = 0x0800_0000
FLASH_SIZE = 192 * 1024
FLASH_WARN = 2 * 1024  # bytes; warn when an image leaves less flash than this
TARGET_DIR = Path("target/thumbv6m-none-eabi")


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


def flash_used(elf: bytes) -> int:
    """Bytes of flash the image occupies: the allocated sections in the flash region, plus the
    load image of the initialised RAM data (`.data`, copied from flash at boot)."""
    e_shoff = struct.unpack_from("<I", elf, 0x20)[0]
    e_shentsize = struct.unpack_from("<H", elf, 0x2E)[0]
    e_shnum = struct.unpack_from("<H", elf, 0x30)[0]
    used = 0
    for i in range(e_shnum):
        _, sh_type, flags, addr, _, size = struct.unpack_from("<IIIIII", elf, e_shoff + i * e_shentsize)
        if not flags & 0x2 or sh_type == 8:  # not SHF_ALLOC, or SHT_NOBITS (.bss, .uninit)
            continue
        if FLASH_BASE <= addr < FLASH_BASE + FLASH_SIZE or RAM_BASE <= addr < RAM_BASE + RAM_SIZE:
            used += size
    return used


def main(bins) -> None:
    if not bins:
        raise SystemExit("usage: check_ram_budget.py <bin-name> [<bin-name> ...]")
    print(f"RAM budget: {RAM_SIZE // 1024} KB SRAM, stack floor {STACK_FLOOR // 1024} KB (flip-link layout)\n")
    print(f"  {'bin':<26} {'statics':>8} {'stack':>8} {'flash free':>10}   verdict")
    failed = False
    for arg in bins:
        name, _, profile = arg.partition("@")
        elf = TARGET_DIR / (profile or "release") / name
        if not elf.exists():
            raise SystemExit(f"{elf} not found; build first (just ram-budget)")
        data = elf.read_bytes()
        top = stack_top(data)
        stack = top - RAM_BASE
        statics = RAM_SIZE - stack
        flash_free = FLASH_SIZE - flash_used(data)
        ok = stack >= STACK_FLOOR
        failed |= not ok
        note = "OK" if ok else f"FAIL (< {STACK_FLOOR} floor)"
        if flash_free < FLASH_WARN:
            note += f", WARN: flash within {FLASH_WARN} B of full"
        print(f"  {name:<26} {statics:>8} {stack:>8} {flash_free:>10}   {note}")
    if failed:
        print(
            "\nRAM budget exceeded: a bin's statics grew until the stack fell below the floor.\n"
            "Trim resident state (async task futures dominate; see docs/firmware/gateway.md), or, if the\n"
            "growth is justified, raise STACK_FLOOR after re-measuring the stack high-water mark."
        )
        sys.exit(1)
    print("\nRAM budget OK")


if __name__ == "__main__":
    main(sys.argv[1:])
