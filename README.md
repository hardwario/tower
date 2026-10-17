# TOWER

**The HARDWARIO TOWER software stack in one repository:** the shared wire protocol, the
Embassy firmware SDK and product apps for the Core Module, the host `tower` CLI, the STM32
bootloader engine and the hardware-in-the-loop bench harness.

TOWER is HARDWARIO's wireless, modular IoT kit. The Core Module is an **STM32L083CZ** (Arm
Cortex-M0+) with a **SPIRIT1 sub-GHz radio** (EU 868 / US 915 MHz, AES-128-CCM secured). The
firmware emits a **framed serial console**; the `tower` CLI decodes and renders it, flashes
boards over the STM32 UART bootloader, and bridges the radio network to MQTT. Firmware is
loaded over the wire (`tower flash`, or SWD); there is no over-the-air update path, by design.

## Layout

| Path | Package | Role |
|------|---------|------|
| [`crates/protocol`](crates/protocol) | `tower-protocol` | The wire contract: COBS + CRC-32 framing and the postcard console, management and radio schemas (`no_std`). |
| [`crates/kv`](crates/kv) | `tower-kv` | Power-loss-safe EEPROM key-value codec behind the SDK's storage (`no_std`, hardware-free). |
| [`crates/net`](crates/net) | `tower-net` | Decision kernels of the radio network layer's security invariants (replay, nonce, ACK, FHSS epoch, pairing). |
| [`crates/radio`](crates/radio) | `tower-radio` | Radio timing / CCM / compliance logic the SDK builds on (duty cycle, time-on-air, FHSS hops); not the SPIRIT1 driver. |
| [`crates/gateway`](crates/gateway) | `tower-gateway` | Decision core of the Radio Dongle gateway firmware (node registry, downlink queue); not the `tower gateway` command. |
| [`crates/shell`](crates/shell) | `tower-shell` | Pure kernels behind the device shell (address parsing, pagination, TAB completion). |
| [`crates/boot`](crates/boot) | `tower-boot` | STM32 ROM UART-bootloader control: flash / erase / verify, chip info, NRST/BOOT0 reset pulses. Formerly the `jolt` flasher. |
| [`firmware/`](firmware) | `tower-sdk` (lib `tower`) | Embassy firmware SDK, `examples/` and the TOWER IoT Kit product `apps/`. Its own Cargo workspace for `thumbv6m-none-eabi`. |
| [`cli/`](cli) | `tower-cli` (bin `tower`) | Host CLI/TUI: logs, events, shell, flash / erase / reset / chip, `tower gateway` (radio to MQTT), `tower nodes`, `tower net`. |
| [`hil/`](hil) | `tower-hil` | HIL bench harness: builds images from `firmware/`, flashes real boards, decodes the console natively, measures power (PPK2). |

**Naming rule:** the directory `crates/X` holds the package `tower-X`. Short directories,
prefixed packages, so the crate names (`tower_net`, `tower_radio`, ...) can't be confused with
the SDK's own modules (`tower::radio`, `tower::shell`) or the `tower` commands.

Siblings depend on each other **by path**: the firmware, the CLI and the HIL harness all build
the one `crates/protocol` tree, so the wire format cannot drift between them at build time
(postcard is not self-describing; this used to be a tag-pinning discipline across five
repositories, whose full histories were imported here).

```
tower/
├── Cargo.toml            host workspace: crates/*, cli, hil (firmware/ excluded)
├── Cargo.lock            host lockfile (firmware/Cargo.lock is the firmware's own)
├── rust-toolchain.toml   the one toolchain (1.99.1 + rustfmt, clippy, llvm-tools, thumbv6m)
├── rustfmt.toml          the one house style (110 columns), applied everywhere
├── justfile              just check / test / fw / hil
├── crates/               protocol, kv, net, radio, gateway, shell, boot
├── firmware/             tower-sdk: src/, examples/, apps/, docs/, tools/ (own workspace)
├── cli/                  tower-cli: the `tower` binary
├── hil/                  tower-hil: the bench harness
├── AGENTS.md             the operating manual for coding agents (CLAUDE.md imports it)
├── .claude/skills/       /bootstrap, /build
└── .github/              CI, the CLI release workflow, Dependabot
```

## Quick start

Prerequisites: [rustup](https://rustup.rs) (`rustup toolchain install` in the checkout installs
the toolchain pinned in `rust-toolchain.toml`), [`just`](https://just.systems), `python3` >= 3.11,
and for the firmware `cargo install --locked flip-link cargo-binutils`. On Linux also
`libudev-dev` + `pkg-config`.

```bash
git clone git@github.com:hardwario/tower.git
cd tower

just check                         # every hardware-free gate (what CI runs)
just test                          # host workspace tests

cargo install --locked --path cli  # the `tower` CLI on your PATH (= just install-cli)
tower devices                      # attached serial devices

just fw examples                   # firmware example names
just fw build example blinky       # -> firmware/target/firmware.bin
just fw flash example blinky       # build + flash over the UART bootloader (board attached)
tower logs                         # decoded console
```

Bench runs (`just hil hil`, `hil-power`, `hil-full`) flash real hardware; see
[`hil/README.md`](hil/README.md). Working with [Claude Code](https://claude.com/claude-code)?
Open it here and run `/bootstrap`; `AGENTS.md` gives it the whole-stack context.

## Releases

- `cli-vX.Y.Z` tags release the `tower` binary: cross-platform archives
  (`tower-cli-vX.Y.Z-<target>`) with checksums and signed build-provenance attestations
  (`gh attestation verify <archive> --repo hardwario/tower`).
- `protocol-vX.Y.Z` tags mark wire-format releases of `crates/protocol`.
- The other crates are internal path dependencies and are not released on their own.

## Contributing

- Develop straight on `main`. A wire-format change is one commit across `crates/protocol` and
  its callers; see the runbook in [`AGENTS.md`](AGENTS.md).
- Security issues: see [`SECURITY.md`](SECURITY.md) (private vulnerability reporting).
- No em or en dashes in code, comments, docs, UI texts or commit messages.

## License

MIT, © 2026 HARDWARIO a.s.; see [`LICENSE`](LICENSE).
