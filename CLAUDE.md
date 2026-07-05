# CLAUDE.md — TOWER control plane

This repository is the **control plane** for the HARDWARIO TOWER ecosystem. You
(Claude) are expected to drive work across five child repositories from this one
session. The children are Git **submodules** at the repo root:

| Submodule  | Upstream                                   | Role |
|------------|--------------------------------------------|------|
| `protocol` | `github.com/hardwario/tower-protocol`      | Shared `no_std` wire-format crate — the contract |
| `firmware` | `github.com/hardwario/tower-firmware`      | Rust/Embassy firmware SDK and product apps |
| `cli`      | `github.com/hardwario/tower-cli`           | Host-side `tower` CLI/TUI |
| `jolt`     | `github.com/hardwario/jolt`                | STM32L0 UART-bootloader flasher (library) used by `cli` + `hil` |
| `hil`      | `github.com/hardwario/tower-hil`           | HIL bench harness (std host crate; builds images from `firmware/`) |

Each child has its **own `CLAUDE.md`**. When you work inside a child, read and
obey that file — it is authoritative for that repo. This file governs how the
pieces fit together and how to change more than one at once.

---

## What TOWER is

TOWER is HARDWARIO's wireless, modular IoT kit. The Core Module is an **STM32L083CZ**
(Arm Cortex-M0+) with a **SPIRIT1 sub-GHz radio** (EU 868 / US 915 MHz, AES-128-CCM
secured network with confirmed delivery, replay protection, OTA pairing). The
firmware streams a **framed serial console** to a host, where the `tower` CLI decodes
and renders it.

### Data flow

```
                 tower-protocol  (COBS + CRC-32 + postcard frames)
                 ▲                                              ▲
                 │ pins tag                                     │ pins same tag
   ┌─────────────┴───────────────┐                ┌─────────────┴───────────────┐
   │  tower-firmware (device)     │   USB serial   │  tower-cli (host)            │
   │  STM32L083CZ · Embassy       │ ──── frames ──▶ │  decodes logs/events/shell   │
   │  emits framed console        │ ◀─── shell ──── │  flashes (UART)              │
   └──────────────────────────────┘                └──────────────────────────────┘
```

The wire format lives in **one place** (`protocol`) and both ends depend on it. So does
`hil`, the bench harness: it decodes the same framed console natively to assert on typed
frames, and builds the images it flashes from the `firmware/` checkout next door.

---

## 🔒 The golden rule: protocol lockstep

`firmware`, `cli`, and `hil` each pin `tower-protocol` **by git tag** in their `Cargo.toml`.
**They must all pin the *same* tag.** The wire format is `postcard`, which is **not
self-describing**: a tag mismatch does not error — it **silently mis-decodes** bytes.
This has bitten production before.

- Current alignment: **`protocol` = v1.2.1**, pinned as `v1.2.1` by `firmware`, `cli`,
  and `hil`. ✅
- `firmware` references `tower-protocol` in **two** manifests: `firmware/Cargo.toml` and
  `firmware/crates/tower-kv/Cargo.toml`.
- `cli` references it in `cli/Cargo.toml`, `hil` in `hil/Cargo.toml` (one place each).
- Any change to the wire format (struct/enum field order, `MsgType` discriminants,
  frame layout) **must** bump `PROTOCOL_VERSION` in `protocol/src/lib.rs`.

**Never bump the protocol tag in one consumer without the others, in the same change-set.**
`/lockstep` is the dedicated check (`tools/check_lockstep.py` — tag pins **and** resolved
lockfile SHAs, so a re-cut tag is caught too); `/sync` and `/pin` run it as their gate.

---

## Per-repo cheat sheet

### `protocol/` — the contract (`no_std`, edition 2024, v1.2.1, MIT)
- Defines: COBS framing + CRC-32 (`src/lib.rs`, `src/crc.rs`) and the postcard message
  schema (`src/msg.rs`: `Hello`, `Log`, `Print`, `Event`, `ShellCommand`, …).
- Verify locally:
  ```bash
  cargo test --manifest-path protocol/Cargo.toml
  cargo clippy --manifest-path protocol/Cargo.toml --all-targets -- -D warnings
  cargo build --manifest-path protocol/Cargo.toml --target thumbv6m-none-eabi
  ```
- Has CI (`.github/workflows/ci.yml`): host test, embedded build, **auto-tags** a
  GitHub release when `Cargo.toml` version is new. Tag manually anyway so consumers
  can be bumped immediately.

### `firmware/` — device SDK (`no_std`, edition 2024, v0.1.0, MIT)
- Target: **`thumbv6m-none-eabi`** (Cortex-M0+). Uses **`just`** as the task runner.
- Workspace members: `crates/tower-kv` (EEPROM key-value codec), `crates/tower-radio-core`
  (host-testable radio-timing/compliance math), `crates/tower-net-core` (host-testable
  security decision kernels: replay rule, TX-counter watermark/fail-closed, ACK dedup,
  FHSS epoch gate, nonce construction). `examples/` = demos, `apps/` = product
  firmwares. (The HIL harness lives in the `hil/` submodule, its own repo.)
- Common commands (read `firmware/justfile` for the full set):
  ```bash
  just -f firmware/justfile examples            # list example names
  just -f firmware/justfile build example blinky
  just -f firmware/justfile flash example blinky   # needs `tower` CLI on PATH + hardware
  just -f firmware/justfile run   example blinky   # build + flash + stream console
  just -f firmware/justfile test                # host tests (tower-kv, tower-net-core, tower-radio-core)
  ```
  Or run inside the dir: `cd firmware && just <recipe>`.
- **Do NOT** assume a plain `cargo build` for firmware — it is an embedded target with
  a custom runner and linker scripts. Go through `just`.
- Flashing/console requires the **`tower` CLI** (the `cli/` submodule) on `PATH` and
  physical hardware. CI (`.github/workflows/ci.yml`) runs the embedded build (incl. role-gated
  example variants), host tests, clippy, and the **tower-protocol lockstep** job (which fetches
  the `tower-cli` **and** `tower-hil` pins). `cli` mirrors the gate (a `lockstep` CI job plus
  a release-workflow gate); `hil` mirrors it too (added 2026-07-05). Local check: `/lockstep`.

### `cli/` — host tool (`tower` binary, edition 2024, v1.0.0, MIT)
- Single crate. Stack: clap 4, ratatui, serialport, rustyline. Depends on
  `tower-protocol` (wire codec) **and the `jolt/` submodule** (`v1.4.0`, the STM32L0
  UART-bootloader flasher), which it links as a **library** (`jolt::firmware::load`,
  `jolt::port::Port`, `jolt::flash::FlashOptions`) for `flash`/`erase`/`reset`.
- Common commands:
  ```bash
  cargo build --manifest-path cli/Cargo.toml --release    # binary: cli/target/release/tower
  cargo fmt   --manifest-path cli/Cargo.toml --all -- --check
  cargo clippy --manifest-path cli/Cargo.toml --all-targets -- -D warnings
  cargo test  --manifest-path cli/Cargo.toml
  ```
- Runtime: `tower` (TUI on auto-detected port), `tower logs|events|shell|monitor`,
  `tower flash <bin>`, `tower reset [--bootloader]`.
- Linux build needs `libudev-dev` + `pkg-config`. Has CI (test + multi-platform
  release archives on `v*` tags). **Not on crates.io.**

### `jolt/` — UART flasher (lib + `jolt` binary, edition 2024, v1.4.0, MIT)
- "Tiny Rust CLI that flashes an STM32L083CZ over the UART bootloader." Modules:
  `bootloader`, `commands`, `firmware`, `flash`, `port`, `target`. `cli` consumes its
  **library** API; it is also usable standalone as the `jolt` binary.
- Pinned by `cli` **and `hil`** via git tag (`jolt = { git = "…/jolt", tag = "v1.4.0" }`).
  Unlike the `tower-protocol` lockstep, a tag mismatch here is a **compile error** in the
  consumer, not a silent failure — so it is a normal dependency bump, not an interop
  hazard. Keep the two consumers' tags equal anyway (`/lockstep` flags a divergence,
  informationally).
- Build/test from the root: `cargo build --manifest-path jolt/Cargo.toml` /
  `cargo test --manifest-path jolt/Cargo.toml`. Linux needs `libudev-dev` + `pkg-config`
  (same serial-port dependency as `cli`). Has its own CI + tagged releases.

### `hil/` — HIL bench harness (std host crate, edition 2024, v0.1.0, MIT)
- Drives the physical bench: a TOWER Core Module (J-Link SWD + Nordic PPK2) and a TOWER
  Radio Dongle (USB). Decodes the framed console **natively** (`tower-protocol`) and asserts
  on typed Log/Event frames + seq gaps; uses `jolt` as a library for the reset pulses.
- **Builds the images it flashes from the `firmware/` checkout next door** (default
  `../firmware` relative to `hil/` — exactly this control-plane layout; `TOWER_FIRMWARE_DIR`
  overrides). The bench roster is `hil/hil.toml`, re-resolved against `tower devices`.
- Compile-check from the root (no hardware): `cargo test --manifest-path hil/Cargo.toml --no-run`.
  Bench runs happen **inside** `hil/` via `just hil` / `just hil-power` / `just hil-full` —
  they flash real hardware; only run them when the user asks and the bench is cabled.
- All hardware tests are `#[ignore]`d; its CI only compile-checks + clippy. Linux needs
  `libudev-dev` + `pkg-config`.

---

## Slash commands (this repo's workflow)

| Command       | Use it to… |
|---------------|------------|
| `/bootstrap`  | Set up a fresh clone: init submodules, verify toolchains. |
| `/build`      | Compile in dependency order (`protocol → jolt → cli → firmware → hil`), or one repo. |
| `/sync`       | Pull each submodule to upstream `main`, then verify protocol lockstep. |
| `/lockstep`   | Just the golden-rule check: protocol tag pins + resolved lockfile SHAs, local trees. |
| `/pin`        | Freeze the current submodule SHAs as a committed known-good snapshot. |

Definitions live in `.claude/commands/`. The mental model: **`/bootstrap` provisions**,
**`/build` compiles**, **`/sync` advances** the working trees toward upstream,
**`/lockstep` checks** the wire-format invariant, and **`/pin` freezes** the trees into a
control-plane commit you can return to. Typical loop: `/sync` → `/build` → `/pin`.

---

## Submodule mechanics (handle gitlinks correctly)

- The control plane stores each child as a **gitlink** — a recorded SHA, not the files.
  `git status` in the root shows a child as modified when its checked-out SHA differs
  from the recorded one.
- To record new child SHAs, stage the gitlink: `git add firmware cli protocol jolt hil`,
  then commit (this is what `/pin` does). The commit message should list each SHA.
- **Never** `git add` a child's *file contents* from the root — changes to a child are
  committed *inside that child* and pushed to its own upstream first.
- **Never pin a child to a commit that isn't pushed** to its remote — a fresh clone
  could not fetch it. `/pin` guards against this.
- Develop a child by `cd`-ing into it (or `git -C <child>`); it is a full clone of the
  upstream repo on its own branch.

---

## Cross-repo runbook: bumping the wire protocol

The highest-stakes operation. Do it as one coordinated change-set:

1. **In `protocol/`**: make the wire change. If the format changed at all, bump
   `PROTOCOL_VERSION` in `src/lib.rs`. Bump `version` in `protocol/Cargo.toml`
   (minor for a wire/behaviour change, patch for a fix). Run the `protocol` test +
   clippy + embedded builds above. Commit, push `main`, then tag:
   `git -C protocol tag -a vX.Y.Z -m "…" && git -C protocol push origin main vX.Y.Z`.
2. **In `firmware/`**: update the tag to `vX.Y.Z` in **both** manifests listed
   above, then `cargo update -p tower-protocol`. Rebuild/test via `just`.
3. **In `cli/`**: update the tag to `vX.Y.Z` in `cli/Cargo.toml`, then
   `cargo update -p tower-protocol`. Build + test. Note: `cli` may have a local
   `.cargo/config.toml` `paths` override shadowing the git source — move it aside
   before `cargo update` so the lockfile re-resolves (see `cli/CLAUDE.md`).
4. **In `hil/`**: update the tag to `vX.Y.Z` in `hil/Cargo.toml`, then
   `cargo update -p tower-protocol` and `cargo test --no-run` (compile-check; no bench).
5. Commit firmware, cli, and hil (in their own repos), push. Run `/lockstep`.
6. **Back here**: `/pin` to record the new, aligned SHAs as a snapshot.

### Local co-development (no re-tagging)

Because the repos sit side by side under this root, you can test a local
`protocol` change against `firmware`/`cli`/`hil` without tagging, via a cargo `paths`
override pointing at `protocol/`. Keep such overrides **local and uncommitted**
(the root `.cargo/config.toml` is git-ignored here for exactly this). Remove the
override and go back to the pinned tag before you `/pin` or hand off.

---

## Conventions (inherited from the children)

- **The TOWER repos develop straight on `main`.** No feature branches unless the
  user explicitly asks. **Commit and push only when the user requests it** — this
  applies to the children *and* to this control plane.
- License is **MIT** across the ecosystem (© 2026 HARDWARIO a.s.).
- Pushes to children use SSH (`git@github.com:hardwario/…`).
- Regulatory citations in firmware radio code (e.g. FCC §15.247, EU duty-cycle) are
  real and load-bearing — **never** remove or alter them.
- `protocol` and `hil` are hand-formatted (no fmt gate) — respect existing style;
  `firmware`, `cli`, and `jolt` are rustfmt-gated in CI (firmware at 110 cols per its
  `rustfmt.toml`). Everywhere: the design-rationale comments are load-bearing — don't
  strip them.

## Don't

- Don't bump `tower-protocol` in one consumer without the others (`firmware`, `cli`, `hil`).
- Don't change the wire format without bumping `PROTOCOL_VERSION`.
- Don't `git add`/commit a child's files from the control-plane root.
- Don't `/pin` a child commit that hasn't been pushed upstream.
- Don't run a bare `cargo build` against `firmware` — use `just`.
- Don't run `hil`'s bench recipes (`just hil*`) — they flash hardware — unless the user asks.
- Don't commit or push anything unless the user asked.
