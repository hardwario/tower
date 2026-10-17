# TOWER

**Control plane for the HARDWARIO TOWER ecosystem.**

This repository orchestrates the repos behind TOWER (the firmware, the host CLI, the
shared wire protocol, the UART flasher, and the HIL bench harness) as Git
**submodules**, so you can drive the whole system from a single
[Claude Code](https://claude.com/claude-code) session with full cross-repo context. It
holds no product code of its own; it holds the *knowledge* of how the pieces fit
together and the workflow to keep them in sync.

```
tower/                ← you are here (the control plane)
├── protocol/         → github.com/hardwario/tower-protocol   (the contract)
├── firmware/         → github.com/hardwario/tower-firmware    (the device)
├── cli/              → github.com/hardwario/tower-cli         (the host tool)
├── jolt/             → github.com/hardwario/jolt              (UART flasher, used by cli + hil)
├── hil/              → github.com/hardwario/tower-hil         (bench harness)
├── AGENTS.md         the operating manual for coding agents across all five
├── CLAUDE.md         `@AGENTS.md` import, so Claude Code loads the same manual
└── .claude/skills/   /bootstrap · /build · /sync · /lockstep · /pin
```

## The ecosystem

TOWER is HARDWARIO's wireless, modular IoT kit. The Core Module is an **STM32L083CZ**
(Arm Cortex-M0+) with a **SPIRIT1 sub-GHz radio** (EU 868 / US 915 MHz, AES-128-CCM
secured). The firmware emits a **framed serial console**; the `tower` CLI decodes and
renders it. Firmware is loaded **over the wire** (USB UART bootloader via `tower
flash`, or SWD); there is no over-the-air update path, by design.

| Repo | Role | Lang | Version |
|------|------|------|---------|
| [**tower-protocol**](https://github.com/hardwario/tower-protocol) | Shared `no_std` wire-format crate: COBS+CRC framing, postcard message schema. The single source of truth the other repos depend on. | Rust | `1.5.1`¹ |
| [**tower-firmware**](https://github.com/hardwario/tower-firmware) | Embassy-based firmware SDK and ready-made product apps for the STM32L0 Core Module. | Rust | `0.1.0` |
| [**tower-cli**](https://github.com/hardwario/tower-cli) | Host-side `tower` CLI/TUI: streams logs/events, an interactive shell, flashes over UART. | Rust | `2.0.0` |
| [**jolt**](https://github.com/hardwario/jolt) | STM32L0 UART-bootloader flasher. `tower-cli` and `tower-hil` link it as a library; also usable standalone. | Rust | `1.5.0` |
| [**tower-hil**](https://github.com/hardwario/tower-hil) | Hardware-in-the-loop bench harness: flashes real boards, decodes the framed console natively, asserts on typed frames, measures power (PPK2). | Rust | `0.1.0` |

¹ The tower-protocol tag every consumer pins at the time of writing; `/lockstep` prints the
current one. All five repos build with Rust **1.99.1**, pinned in each `rust-toolchain.toml`
(CI installs exactly that toolchain).

```
                 tower-protocol  (COBS + CRC + postcard frames)
                 ▲                  ▲                        ▲
                 │ same git tag     │ same git tag           │ same git tag
   ┌─────────────┴──────────────┐   │           ┌────────────┴───────────────┐
   │  tower-firmware (device)    │   │  USB/UART  │  tower-cli (host)           │
   │  STM32L083CZ · Embassy      │───┼─ frames ─▶│  decode logs/events/shell   │
   │  framed console             │◀──┼─ shell ───│  flash · reset              │
   └─────────────▲───────────────┘   │           └─────────────────────────────┘
                 │ builds + flashes  │
   ┌─────────────┴───────────────────┴─────┐
   │  tower-hil (bench)                     │
   │  J-Link SWD · PPK2 power · radio dongle│
   └────────────────────────────────────────┘
```

> **The one rule that matters:** `firmware`, `cli`, and `hil` must pin the **same**
> `tower-protocol` tag. The wire format is `postcard` (not self-describing), so a
> mismatch *silently* mis-decodes instead of erroring. `/lockstep` is the dedicated
> check (every manifest's tag pin, the resolved lockfile SHAs, and the commit the tag
> names); `/sync` and `/pin` run it as their gate, and CI enforces it in every repo.

## Quick start

```bash
# Clone with the children in one shot
git clone --recurse-submodules git@github.com:hardwario/tower.git
cd tower

# Then open Claude Code here and run:
/bootstrap          # init submodules, verify toolchains, report status
```

Already cloned without `--recurse-submodules`? Just run `/bootstrap`: it initializes
the missing submodules for you (and leaves already checked-out ones alone).

## The workflow

Open a Claude Code session **in this directory**. Five skills (invoked as slash
commands, defined in `.claude/skills/`) cover the day-to-day loop:

| Command | What it does |
|---------|--------------|
| **`/bootstrap`** | First-run setup: initialize missing submodules, check the pinned Rust toolchain (targets + components) and what the firmware needs (`just`, `flip-link`, `cargo-binutils`, the `tower` CLI; `probe-rs` only for SWD). Reports missing tools; installs nothing but rustup targets/components. |
| **`/build`** | Compile in dependency order (`protocol → jolt → cli → firmware → hil`), or a single repo, `--locked`. Stops at the first failure. Never flashes. |
| **`/sync`** | Pull each submodule to its upstream `main`, then verify the `tower-protocol` lockstep across `firmware`, `cli`, and `hil`. Read-only with `--check`. |
| **`/lockstep`** | Just the golden-rule check: `tower-protocol` pins in every manifest + resolved lockfile SHAs + the tag's commit, across the local trees (catches a re-cut tag too). Read-only. |
| **`/pin`** | Freeze the current submodule commits as a committed, known-good snapshot (with safety checks: clean trees, every pinned commit on its upstream `main`, strict lockstep). Optionally `--tag`. Never pushes. |

Typical cycle: **`/sync`** → **`/build`** the affected repos → **`/pin`** to record
the new known-good combination.

Beyond the skills, just talk to Claude: *"add a new shell command to the CLI and the
matching handler in the firmware"*, *"bump the protocol to add a humidity event"*, or
*"why does the console show garbage?"*: `AGENTS.md` gives it the cross-repo context to
do it correctly.

## Working on a child repo

Each submodule is a full clone of its upstream on its own `main` branch. Build/test
each from the control-plane root:

```bash
# protocol: the shared crate
cargo test  --manifest-path protocol/Cargo.toml

# cli: the host tool (binary: cli/target/release/tower)
cargo build --manifest-path cli/Cargo.toml --release

# firmware: embedded; uses `just`, not bare cargo (recipes run inside firmware/)
just -f firmware/justfile check                  # lib + apps + examples, like CI
just -f firmware/justfile build example blinky   # → firmware/target/firmware.bin

# jolt: host UART flasher that cli + hil link as a library
cargo build --manifest-path jolt/Cargo.toml

# hil: bench harness; compile-check without hardware
cargo test --manifest-path hil/Cargo.toml --no-run --features power
```

Firmware flashing and the console require the `tower` CLI on your `PATH` and a
physical Core Module. Bench runs (`just hil` inside `hil/`) flash real hardware;
see `hil/README.md`. See each child's own `README.md`/`AGENTS.md` for depth.

> **Note:** `jolt` is pinned by git tag (`v1.5.0`) by both `cli` and `hil`, just like
> `tower-protocol`. But because they link it as a Rust **library**, a tag mismatch is
> a compile error rather than the *silent* mis-decode that a protocol-tag mismatch
> causes, so it carries no lockstep hazard.

## Repository layout

```
tower/
├── README.md                 this file
├── AGENTS.md                 cross-repo operating manual for coding agents
├── CLAUDE.md                 `@AGENTS.md` import (Claude Code entry point)
├── SECURITY.md               how to report a vulnerability (private reporting)
├── .gitmodules               submodule definitions (HTTPS URLs, cloneable by anyone)
├── .claude/skills/           /bootstrap, /build, /sync, /lockstep, /pin (SKILL.md each)
├── .github/                  CI (recursive checkout + strict lockstep) + Dependabot
├── tools/check_lockstep.py   the /lockstep implementation
├── protocol/   (submodule)   github.com/hardwario/tower-protocol
├── firmware/   (submodule)   github.com/hardwario/tower-firmware
├── cli/        (submodule)   github.com/hardwario/tower-cli
├── jolt/       (submodule)   github.com/hardwario/jolt
└── hil/        (submodule)   github.com/hardwario/tower-hil
```

Submodules are pinned by commit SHA, so a clone of this repo reproduces an exact,
coherent combination of all five. `/pin` advances those pins; `/sync` brings the
working trees up to upstream before you pin.

## Contributing & conventions

- The TOWER repos develop **straight on `main`**; no feature branches unless asked.
- Changes to a child are committed **inside that child** and pushed to its own
  upstream; never `git add` a child's files from this root. This repo only records
  the resulting SHAs (that's `/pin`).
- A wire-format change is a coordinated change-set across `protocol`, `firmware`,
  `cli`, and `hil`; see the runbook in [`AGENTS.md`](./AGENTS.md).
- Licensed **MIT** (© 2026 HARDWARIO a.s.), consistent with the child repos.

## License

MIT; see the child repositories for their respective `LICENSE` files.
