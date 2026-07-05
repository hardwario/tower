# TOWER

**Control plane for the HARDWARIO TOWER ecosystem.**

This repository orchestrates the repos behind TOWER — the firmware, the host CLI, the
shared wire protocol, the UART flasher, and the HIL bench harness — as Git
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
├── CLAUDE.md         the operating manual for Claude across all five
└── .claude/commands/ /bootstrap · /build · /sync · /lockstep · /pin
```

## The ecosystem

TOWER is HARDWARIO's wireless, modular IoT kit. The Core Module is an **STM32L083CZ**
(Arm Cortex-M0+) with a **SPIRIT1 sub-GHz radio** (EU 868 / US 915 MHz, AES-128-CCM
secured). The firmware emits a **framed serial console**; the `tower` CLI decodes and
renders it. Firmware is loaded **over the wire** (USB UART bootloader via `tower
flash`, or SWD) — there is no over-the-air update path, by design.

| Repo | Role | Lang | Version |
|------|------|------|---------|
| [**tower-protocol**](https://github.com/hardwario/tower-protocol) | Shared `no_std` wire-format crate: COBS+CRC framing, postcard message schema. The single source of truth the other repos depend on. | Rust | `1.2.1` |
| [**tower-firmware**](https://github.com/hardwario/tower-firmware) | Embassy-based firmware SDK and ready-made product apps for the STM32L0 Core Module. | Rust | `0.1.0` |
| [**tower-cli**](https://github.com/hardwario/tower-cli) | Host-side `tower` CLI/TUI: streams logs/events, an interactive shell, flashes over UART. | Rust | `1.0.0` |
| [**jolt**](https://github.com/hardwario/jolt) | STM32L0 UART-bootloader flasher. `tower-cli` and `tower-hil` link it as a library; also usable standalone. | Rust | `1.4.0` |
| [**tower-hil**](https://github.com/hardwario/tower-hil) | Hardware-in-the-loop bench harness: flashes real boards, decodes the framed console natively, asserts on typed frames, measures power (PPK2). | Rust | `0.1.0` |

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
> check (tag pins **and** resolved lockfile SHAs); `/sync` and `/pin` run it as
> their gate.

## Quick start

```bash
# Clone with the children in one shot
git clone --recurse-submodules git@github.com:hardwario/tower.git
cd tower

# Then open Claude Code here and run:
/bootstrap          # init submodules, verify toolchains, report status
```

Already cloned without `--recurse-submodules`? Just run `/bootstrap` — it will
`git submodule update --init` for you.

## The workflow

Open a Claude Code session **in this directory**. Five slash commands cover the
day-to-day loop:

| Command | What it does |
|---------|--------------|
| **`/bootstrap`** | First-run setup: materialize submodules, check Rust toolchains/targets (and `probe-rs` for firmware). |
| **`/build`** | Compile in dependency order (`protocol → jolt → cli → firmware → hil`), or a single repo. Stops at the first failure. |
| **`/sync`** | Pull each submodule to its upstream `main`, then verify the `tower-protocol` lockstep across `firmware`, `cli`, and `hil`. Read-only with `--check`. |
| **`/lockstep`** | Just the golden-rule check: `tower-protocol` tag pins + resolved lockfile SHAs across the local trees (catches a re-cut tag too). |
| **`/pin`** | Freeze the current submodule commits as a committed, known-good snapshot (with safety checks that every pinned commit is pushed). Optionally `--tag`. |

Typical cycle: **`/sync`** → **`/build`** the affected repos → **`/pin`** to record
the new known-good combination.

Beyond the commands, just talk to Claude: *"add a new shell command to the CLI and the
matching handler in the firmware"*, *"bump the protocol to add a humidity event"*, or
*"why does the console show garbage?"* — `CLAUDE.md` gives it the cross-repo context to
do it correctly.

## Working on a child repo

Each submodule is a full clone of its upstream on its own `main` branch. Build/test
each from the control-plane root:

```bash
# protocol — the shared crate
cargo test  --manifest-path protocol/Cargo.toml

# cli — the host tool (binary: cli/target/release/tower)
cargo build --manifest-path cli/Cargo.toml --release

# firmware — embedded; uses `just`, not bare cargo
cd firmware && just examples && just build example blinky

# jolt — host UART flasher that cli + hil link as a library
cargo build --manifest-path jolt/Cargo.toml

# hil — bench harness; compile-check without hardware
cargo test --manifest-path hil/Cargo.toml --no-run
```

Firmware flashing and the console require the `tower` CLI on your `PATH` and a
physical Core Module. Bench runs (`just hil` inside `hil/`) flash real hardware —
see `hil/README.md`. See each child's own `README.md`/`CLAUDE.md` for depth.

> **Note:** `jolt` is pinned by git tag (`v1.4.0`) by both `cli` and `hil`, just like
> `tower-protocol`. But because they link it as a Rust **library**, a tag mismatch is
> a compile error rather than the *silent* mis-decode that a protocol-tag mismatch
> causes — so it carries no lockstep hazard.

## Repository layout

```
tower/
├── README.md                 this file
├── CLAUDE.md                 cross-repo operating manual for Claude
├── .gitmodules               submodule definitions (HTTPS URLs, cloneable by anyone)
├── .claude/commands/         /bootstrap, /build, /sync, /lockstep, /pin
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
  upstream — never `git add` a child's files from this root. This repo only records
  the resulting SHAs (that's `/pin`).
- A wire-format change is a coordinated change-set across `protocol`, `firmware`,
  `cli`, and `hil` — see the runbook in [`CLAUDE.md`](./CLAUDE.md).
- Licensed **MIT** (© 2026 HARDWARIO a.s.), consistent with the child repos.

## License

MIT — see the child repositories for their respective `LICENSE` files.
