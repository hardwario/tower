# tower-hil

The hardware-in-the-loop bench harness: it builds images from `firmware/`, flashes real boards,
decodes their consoles and measures power (PPK2).

- [The harness](../docs/hil/hil.md): layout, running, the wire format.
- [The bench](../docs/hil/bench.md): inventory, cabling, setup, troubleshooting.

`just hil check` compiles it without hardware; the bench recipes (`just hil bench*`) flash boards.
