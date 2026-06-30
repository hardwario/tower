# TOWER

**Control plane for the HARDWARIO TOWER ecosystem.**

This repository orchestrates the three repos that make up TOWER — the firmware, the
host CLI, and the shared wire protocol — as Git **submodules**, so you can drive the
whole system from a single [Claude Code](https://claude.com/claude-code) session with
full cross-repo context. It holds no product code of its own; it holds the *knowledge*
of how the pieces fit together and the workflow to keep them in sync.

```
tower/                ← you are here (the control plane)
├── protocol/         → github.com/hardwario/tower-protocol   (the contract)
├── firmware/         → github.com/hardwario/tower-firmware    (the device)
├── cli/              → github.com/hardwario/tower-cli         (the host tool)
├── CLAUDE.md         the operating manual for Claude across all three
└── .claude/commands/ /bootstrap · /sync · /pin
```

## The ecosystem

TOWER is HARDWARIO's wireless, modular IoT kit. The Core Module is an **STM32L083CZ**
(Arm Cortex-M0+) with a **SPIRIT1 sub-GHz radio** (EU 868 / US 915 MHz, AES-128-CCM
secured). The firmware emits a **framed serial console**; the `tower` CLI decodes and
renders it; firmware updates ship over-the-air as **Ed25519-signed images**.

| Repo | Role | Lang | Version |
|------|------|------|---------|
| [**tower-protocol**](https://github.com/hardwario/tower-protocol) | Shared `no_std` wire-format crate: COBS+CRC framing, postcard message schema, signed FOTA manifest. The single source of truth both other repos depend on. | Rust | `1.0.0` |
| [**tower-firmware**](https://github.com/hardwario/tower-firmware) | Embassy-based firmware SDK, ready-made product apps, and the A/B FOTA bootloader for the STM32L0 Core Module. | Rust | `0.1.0` |
| [**tower-cli**](https://github.com/hardwario/tower-cli) | Host-side `tower` CLI/TUI: streams logs/events, an interactive shell, flashes over UART, and serves FOTA images. | Rust | `0.2.0` |

```
                 tower-protocol  (frames + FOTA manifest)
                 ▲                                        ▲
                 │ same git tag                           │ same git tag
   ┌─────────────┴──────────────┐            ┌────────────┴───────────────┐
   │  tower-firmware (device)    │  USB/UART  │  tower-cli (host)           │
   │  STM32L083CZ · Embassy      │ ── frames ▶│  decode logs/events/shell   │
   │  framed console + FOTA      │ ◀── shell ─│  flash · reset · fota serve │
   └─────────────────────────────┘            └─────────────────────────────┘
```

> **The one rule that matters:** `firmware` and `cli` must pin the **same**
> `tower-protocol` tag. The wire format is `postcard` (not self-describing), so a
> mismatch *silently* mis-decodes instead of erroring. `/sync` checks this for you.

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

Open a Claude Code session **in this directory**. Three slash commands cover the
day-to-day loop:

| Command | What it does |
|---------|--------------|
| **`/bootstrap`** | First-run setup: materialize submodules, check Rust toolchains/targets (and `probe-rs` for firmware), optionally `--build`. |
| **`/sync`** | Pull each submodule to its upstream `main`, then verify the `tower-protocol` tag is aligned across `firmware` and `cli`. Read-only with `--check`. |
| **`/pin`** | Freeze the current submodule commits as a committed, known-good snapshot (with safety checks that every pinned commit is pushed). Optionally `--tag`. |

Typical cycle: **`/sync`** → build & test the affected repos → **`/pin`** to record
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
cargo test  --manifest-path protocol/Cargo.toml --features verify

# cli — the host tool (binary: cli/target/release/tower)
cargo build --manifest-path cli/Cargo.toml --release

# firmware — embedded; uses `just`, not bare cargo
cd firmware && just examples && just build example blinky
```

Firmware flashing and the console require the `tower` CLI on your `PATH` and a
physical Core Module. See each child's own `README.md`/`CLAUDE.md` for depth.

> **Note:** `tower-cli` additionally depends on [`hardwario/jolt`](https://github.com/hardwario/jolt)
> (`v1.2.0`), the STM32L0 UART bootloader engine. It is a dependency, not part of this
> control plane — add it as a fourth submodule later if it needs co-development here.

## Repository layout

```
tower/
├── README.md                 this file
├── CLAUDE.md                 cross-repo operating manual for Claude
├── .gitmodules               submodule definitions (HTTPS URLs, cloneable by anyone)
├── .claude/commands/         /bootstrap, /sync, /pin
├── protocol/   (submodule)   github.com/hardwario/tower-protocol
├── firmware/   (submodule)   github.com/hardwario/tower-firmware
└── cli/        (submodule)   github.com/hardwario/tower-cli
```

Submodules are pinned by commit SHA, so a clone of this repo reproduces an exact,
coherent combination of all three. `/pin` advances those pins; `/sync` brings the
working trees up to upstream before you pin.

## Contributing & conventions

- All three repos develop **straight on `main`**; no feature branches unless asked.
- Changes to a child are committed **inside that child** and pushed to its own
  upstream — never `git add` a child's files from this root. This repo only records
  the resulting SHAs (that's `/pin`).
- A wire-format change is a coordinated change-set across all three repos — see the
  runbook in [`CLAUDE.md`](./CLAUDE.md).
- Licensed **MIT** (© 2026 HARDWARIO a.s.), consistent with the child repos.

## License

MIT — see the child repositories for their respective `LICENSE` files.
