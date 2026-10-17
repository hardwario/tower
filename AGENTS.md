# AGENTS.md: the TOWER monorepo

This repository (`github.com/hardwario/tower`) holds the whole HARDWARIO TOWER software stack:
the shared wire protocol, the firmware SDK and product apps, the host `tower` CLI, the
bootloader engine and the HIL bench harness. It used to be a control plane over five
submodule repos; their full histories were imported, and every sibling is now a **path
dependency**, so the wire-format lockstep is a compile-time fact instead of a tag-pinning
discipline.

Each component directory below that has an `AGENTS.md` (with a `CLAUDE.md` stub importing it)
carries its own working notes; when you work inside one, read and obey that file too. This
file governs the layout, the naming, and the changes that span several components.

---

## What TOWER is

TOWER is HARDWARIO's wireless, modular IoT kit. The Core Module is an **STM32L083CZ**
(Arm Cortex-M0+, 192 KB flash / 20 KB RAM) with a **SPIRIT1 sub-GHz radio** (EU 868 / US
915 MHz, AES-128-CCM secured network with confirmed delivery, replay protection, OTA pairing).
The firmware streams a **framed serial console** to a host, where the `tower` CLI decodes and
renders it. Firmware is loaded over the wire (the STM32 ROM UART bootloader via `tower flash`,
or SWD); there is no over-the-air update path, by design.

## Layout and naming

**Naming rule:** the directory `crates/X` holds the package `tower-X` (short directories,
prefixed packages; the Rust crate name is `tower_X`).

| Path | Package | What it is |
|------|---------|------------|
| `crates/protocol` | `tower-protocol` | The wire contract (`no_std`): COBS + CRC-32 framing, the postcard console / management / radio schemas, the schema tags derived from them. |
| `crates/kv` | `tower-kv` | Power-loss-safe EEPROM key-value codec behind the SDK's `tower::storage::Kv` (`no_std`, hardware-free). |
| `crates/net` | `tower-net` | Decision kernels of the radio network layer's security invariants (replay, TX-counter watermark, ACK, FHSS epoch, pairing, CCM nonce). Not the network layer itself (`tower::radio::net`). |
| `crates/radio` | `tower-radio` | Radio timing / CCM / compliance logic (duty token buckets, time-on-air, FHSS hop permutation). Not the SPIRIT1 driver (`tower::radio`). |
| `crates/gateway` | `tower-gateway` | Decision core of the Radio Dongle gateway firmware (node-registry codec, downlink queue). Not the host `tower gateway` MQTT bridge (that is in `cli`). |
| `crates/shell` | `tower-shell` | Pure kernels behind the device shell (`addr` parser + PRNG, pagination window, TAB completion). Not the shell itself (`tower::shell`). |
| `crates/flasher` | `tower-flasher` | The flasher, around the STM32 ROM UART bootloader (AN3155): flash / erase / verify, chip info, readout protection, NRST/BOOT0 reset pulses. Library only; its front end is `tower`. |
| `firmware/` | `tower-sdk` (lib `tower`) | Embassy firmware SDK for the Core Module, `examples/` (demos) and `apps/` (TOWER IoT Kit products). Its **own Cargo workspace** (thumbv6m + flip-link). |
| `cli/` | `tower-cli` (bin `tower`) | Host CLI/TUI: logs, events, shell, flash / erase / reset / chip, the `tower gateway` radio-to-MQTT bridge, `tower nodes` / `tower net`. Version 2.0.0. |
| `hil/` | `tower-hil` | Hardware-in-the-loop bench harness: builds images from `../firmware`, flashes real boards, decodes the console natively, measures power (PPK2). |

Two Cargo workspaces:

- **Host workspace** (root `Cargo.toml`): `members = ["crates/*", "cli", "hil"]`, one root
  `Cargo.lock`, `[workspace.package]` (edition 2024, `rust-version` 1.99.1, MIT, repository)
  inherited by every member, shared versions in `[workspace.dependencies]`. The internal library
  crates (`crates/*`) are `version = "0.0.0"`, `publish = false`; `tower-cli` keeps its release
  version (2.0.0) and the firmware SDK its own (shown to humans in the boot `Hello`).
- **Firmware workspace** (`firmware/`, excluded from the host workspace): only the SDK package,
  with `firmware/.cargo/config.toml` (target `thumbv6m-none-eabi`, linker `flip-link`), the
  size-tuned profiles and its own `firmware/Cargo.lock`. It depends on `../crates/*` by path.
  That cargo config applies only when cargo runs **inside** `firmware/`, so never build it with
  `--manifest-path firmware/...` from the root: go through `just fw <recipe>`.

One `rust-toolchain.toml` (channel 1.99.1 + rustfmt, clippy, llvm-tools + thumbv6m), one
`rustfmt.toml` (110 columns, applied to everything: there are no hand-formatted crates any
more), one `LICENSE`, one `SECURITY.md`, one `.github/`.

### Data flow

```
                     crates/protocol  (COBS + CRC-32 + postcard frames)
                     ▲  path dep             ▲ path dep              ▲ path dep
   ┌─────────────────┴──────────┐   ┌────────┴──────────────────┐   ┌┴──────────────────────┐
   │ firmware/ (device)          │   │ cli/  `tower` (host)       │   │ hil/ (bench harness)   │
   │ STM32L083CZ, Embassy        │──▶│ decodes logs/events/shell  │   │ builds ../firmware,    │
   │ framed console over USB UART│◀──│ flash/erase/reset/chip     │   │ flashes, decodes,      │
   │ uses crates/kv net radio    │   │ (crates/flasher), MQTT     │   │ measures (PPK2)        │
   │ gateway shell               │   └────────────────────────────┘   └────────────────────────┘
   └─────────────────────────────┘
```

Every end decodes with the same `crates/protocol` source tree, so a build can never mix
schemas. What can still differ at **run time** is a device flashed, or a `tower` binary
installed, from another checkout: every console frame carries `CONSOLE_TAG` and every radio
envelope `RADIO_TAG`, 16-bit tags derived from the schema's structural hash, so the stale side is
refused (`SchemaMismatch`; `tower` prints a banner naming the device's firmware and exits 125).
The tag can't say which side is older: after a wire change, reflash the boards and reinstall
the CLI (`just install-cli`).

---

## Commands (root `justfile`)

```bash
just check                     # every hardware-free gate CI runs, both workspaces (see below)
just test                      # host workspace tests (cargo test --workspace --locked)
just fw <recipe...>            # a firmware/justfile recipe, run inside firmware/
just hil <recipe...>           # a hil/justfile recipe, run inside hil/
just install-cli               # cargo install --locked --path cli
just fmt                       # rustfmt both workspaces
just fuzz <target> [secs]      # cargo-fuzz one target (nightly + cargo-fuzz; fuzz/README.md), `just fuzz-all` = all
```

`just check` = `check-host` (fmt, clippy `-D warnings`, tests, tower-net's tests on the
firmware's compact SHA-256 backend (`test-sha2-compact`), `tower-hil --features power` clippy +
compile, the no_std build of the shared crates, rustdoc) + `check-firmware` (`just fw ci`: fmt,
MCU build, RAM budget, clippy + the five role variants, rustdoc, example / app list drift; plus
`just fw build example blinky`) + `audit` (`cargo audit` on all three lockfiles). It needs
`flip-link`, `cargo-binutils`, `cargo-audit`, `python3` >= 3.11 and network access for the
advisory DB.

Firmware day to day (`just fw` lists everything): `just fw examples`, `just fw build example
blinky`, `just fw flash example blinky` and `just fw run app radio_push_button` (these two need
the `tower` CLI on `PATH` and a board), `just fw test` (host tests of the shared crates the
firmware builds on). HIL: `just hil check` compiles the harness; `just hil hil`,
`hil-power`, `hil-full` FLASH the bench: only when the user asks and the bench is cabled.

Plain cargo works for the host workspace from the root: `cargo test -p tower-protocol`,
`cargo build --release -p tower-cli` (binary `target/release/tower`). Linux needs
`libudev-dev` + `pkg-config` (serialport), and `perl` + `make` + a C compiler: the CLI links
OpenSSL statically from source (`openssl/vendored`), the TLS stack of its MQTT links there.

## Skills

| Skill | Use it to... |
|-------|--------------|
| `/bootstrap` | Check a fresh clone: rustup + the pinned toolchain (components, thumbv6m), just, flip-link, cargo-binutils, python3 >= 3.11, the `tower` CLI build. Reports; installs only rustup targets/components. |
| `/build` | Run the root gates (`just check`, or one part of it). Hardware-free, never flashes. |

Definitions live in `.claude/skills/<name>/SKILL.md`.

---

## Changing the wire format (the runbook)

postcard is **not self-describing**: field order and variant indices are the wire. There are no
version constants to bump, no compatibility with older peers and no negotiation:

1. **Change the types** in `crates/protocol` (`src/msg.rs`, `src/mgmt.rs`: console;
   `src/radio.rs`: the radio application schema) and every caller in `firmware/`, `cli/` and
   `hil/` in the same commit (they build against the change at once; the compiler finds them).
2. **Every end rebuilds from that checkout**: reflash the boards, reinstall the CLI
   (`just install-cli`). A stale peer is rejected by its tag: `CONSOLE_TAG` / `RADIO_TAG` are
   derived from the schema at compile time, so any change that reaches the bytes moves them.

The framing itself (header, COBS, CRC) is not hashed; `crates/protocol/tests/framing.rs` pins
it. Details: `crates/protocol/AGENTS.md`, "Changing the wire".

## Tags and releases

- `cli-vX.Y.Z` releases the `tower` binary: `.github/workflows/release-cli.yml` checks that the
  tag matches `cli/Cargo.toml`, the commit is on `main` and CI is green for it, then builds
  cross-platform archives (`tower-cli-vX.Y.Z-<target>`) with checksums and signed
  build-provenance attestations.
- `tower-protocol`, `tower-flasher` and the other `crates/*` are internal path dependencies
  (`0.0.0`, `publish = false`): no releases, no tags. The `protocol-v*` tags already pushed
  (the hand-versioned wire releases), the imported `jolt-v*` tags (tower-flasher's history) and
  `cli-v1.0.0` are history; no new `protocol-v*` tags are made.

---

## Conventions

- **Develop straight on `main`.** No feature branches unless the user explicitly asks.
  **Commit, tag and push only when the user requests it.** Pushes use SSH
  (`git@github.com:hardwario/tower.git`).
- License is **MIT** (© 2026 HARDWARIO a.s.), one root `LICENSE`.
- Regulatory citations in the radio code (FCC §15.247, EN 300 220, the EU duty cycle) are real
  and load-bearing: **never** remove or alter them.
- The design-rationale comments are load-bearing everywhere; don't strip them.
- **Toolchain:** `rust-toolchain.toml` pins Rust (channel 1.99.1; `rust-version` is inherited
  from `[workspace.package]`, the firmware sets its own). CI installs exactly that channel via
  the `RUST_TOOLCHAIN` env of each workflow; the `audit` job fails if they differ, so bump them
  together (Dependabot's rust-toolchain PRs need the workflow lines added).
- **Formatting:** one `rustfmt.toml` (110 cols, stable options only; comment content is never
  reflowed), `cargo fmt --all` in both workspaces (`just fmt`), gated in CI.
- **CI / supply chain:** `.github/workflows/ci.yml` (jobs `host`, `host-os`, `firmware`,
  `audit`) and `release-cli.yml`. Every GitHub Action is pinned to a full commit
  SHA with a `# vX.Y.Z` comment (Dependabot bumps them weekly with a 7-day cooldown); workflow
  tokens are read-only by default (`permissions: contents: read`; write only in the release
  `publish` job); checkouts use `persist-credentials: false`; every cargo call is `--locked`;
  `cargo-audit` checks all three lockfiles (host, firmware, fuzz; vulnerabilities fail,
  unmaintained warnings don't).
  Security reports go through GitHub private vulnerability reporting (`SECURITY.md`). The threat
  model, mechanisms and residual risks are in `docs/security-model.md`: update it with any change
  to keys, counters, pairing, the MQTT access policy or readout protection.

## Compatibility: none, until the first users

TOWER (this new generation) has **no users yet**, so there is **no backward-compatibility
obligation anywhere**: wire formats, the radio protocol and pairing, EEPROM layouts, MQTT topics
and payloads, CLI flags and exit codes, settings and crate APIs may all change freely. Don't add
legacy paths, migrations, version negotiation, deprecation periods or "keep the old one for a
release" shims. When something changes, change it everywhere in the same commit. Keep only
mismatch **detection** (the schema tags), so a stale board or an old installed `tower` is caught
instead of silently mis-decoding.

This rule ends when TOWER gets its first users: the root [TODO.md](TODO.md) ("Compatibility
gate") lists what to decide and reintroduce before that, and removing this section is part of it.

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.

## Don't

- Don't add version numbers, compatibility shims or migrations to the wire: change the types,
  rebuild every end from the same checkout, and let the schema tags refuse a stale peer.
- Don't depend on a sibling crate by git tag or registry version; use the workspace path
  dependency (`tower-protocol.workspace = true`, or `{ path = "../crates/..." }` from the
  firmware).
- Don't build the firmware with a bare `cargo build` from the root or with `--manifest-path
  firmware/...`; run cargo inside `firmware/` (`just fw <recipe>`).
- Don't run `hil`'s bench recipes (`just hil hil*`), which flash hardware, unless the user asks.
- Don't reference a GitHub Action by tag or branch, or widen a workflow's `permissions:`; pin
  the full commit SHA (with its `# vX.Y.Z` comment) and grant write only to the job that needs it.
- Don't commit, tag or push anything unless the user asked.
- Don't use em or en dashes anywhere (see *Writing style*).
- Don't add backward-compatibility code (legacy formats, migrations, shims) while TOWER has no
  users (see *Compatibility*).
