# Firmware SDK (`tower-sdk`): working notes for coding agents

Embassy-based, `no_std` firmware SDK for the HARDWARIO TOWER Core Module (STM32L083CZ,
Cortex-M0+), the `firmware/` directory of the hardwario/tower monorepo (read the root
`AGENTS.md` first). The package is `tower-sdk`; its library keeps the crate name `tower`
(`[lib] name = "tower"`), so code writes `use tower::...`. The crate is a library; runnable
programs come in two kinds: educational `examples/` (Cargo `--example`) and TOWER IoT Kit
**products** in `apps/` (Cargo `[[bin]]`, `--bin`). Two apps are complete, HW-verified radio
products: `radio_push_button` (sleeping sensor node) and `radio_dongle_gateway` (radio↔serial
bridge + node coordinator); `radio_climate_monitor` is still a starter skeleton (non-radio logic
runs; radio send is a TODO). Build/flash with `just`, which takes the kind then the name
(`just flash example blinky`, `just build app radio_push_button`, `just run example <name>`)
over the UART bootloader via the `tower` CLI (`../cli`). Standalone device control
(logs/console/reset/erase/chip/devices) is the CLI's own job: call `tower` directly; there are
no `just` wrappers for it. Subsystem guides: `docs/radio.md`, `docs/console.md`,
`docs/storage.md`, `docs/gateway.md` (the push-button + gateway product).

## Workspace and builds

- `firmware/` is **its own Cargo workspace** (the root `Cargo.toml` excludes it), with its own
  `Cargo.lock` and size-tuned profiles. `.cargo/config.toml` here sets the default target
  (`thumbv6m-none-eabi`) and the linker (`flip-link`); cargo reads it from the CURRENT
  directory, so run cargo inside `firmware/` (or through `just` / `just fw <recipe>` from the
  repo root), never `cargo build --manifest-path firmware/Cargo.toml` from elsewhere (that
  silently builds for the host or links without flip-link).
- The toolchain (`rust-toolchain.toml`) and the rustfmt style (`rustfmt.toml`, 110 cols) are the
  repo-root files; they apply here too.
- `just ci` (= CI's `firmware` job) runs every hardware-free firmware gate: fmt, the MCU build
  of lib + apps + examples, the RAM budget, clippy + the five role-feature variants, role
  builds, rustdoc, and the `examples`/`apps` list drift check. `just check` is the MCU build
  alone.
- Host tests: the firmware itself is `no_std` and can't `cargo test`; its logic is tested
  through the shared crates it builds on. `just test` here (or `just fw test` from the repo
  root) runs those crates' host tests (`tower-protocol`, `tower-kv`, `tower-net`, `tower-radio`,
  `tower-gateway`, `tower-shell`); `just test` at the repo root runs the whole host workspace.

## Shared crates (`../crates/*`, path dependencies)

The SDK depends **by path** on the monorepo's shared `no_std` crates: `tower-protocol`
(`../crates/protocol`, the console wire format), `tower-kv` (`../crates/kv`, the EEPROM
key-value codec behind `tower::storage::Kv`), `tower-net` (`../crates/net`, the network-layer
security decision kernels), `tower-radio` (`../crates/radio`, the radio timing / CCM /
compliance arithmetic; not the SPIRIT1 driver, which is `tower::radio`), `tower-gateway`
(`../crates/gateway`, the gateway app's registry / downlink-queue core; not the host-side
`tower gateway` command) and `tower-shell` (`../crates/shell`, the shell's parser / PRNG /
pagination / completion kernels; not `tower::shell` itself). There is nothing to pin: the
`tower` CLI and the HIL harness build the very same `crates/protocol` tree, so the wire format
cannot drift between the ends at build time.

**If you change the protocol:** follow the runbook in `../crates/protocol/AGENTS.md`
("Releasing a change": bump `PROTOCOL_VERSION` or `RADIO_SCHEMA_VERSION`, update the goldens,
one commit, tag `protocol-vX.Y.Z`), fixing the firmware, the CLI and the harness in that same
commit. Then reflash the boards and reinstall the CLI (`just install-cli` at the repo root): an
old `tower` binary reports `BadVersion` against the new firmware.

## Conventions

- `tower-protocol` was developed here (in `src/`) before it was extracted; design rationale +
  hard-won caveats live in the `docs/*.md` guides and in code comments (don't strip them).
- Radio is regulatory-sensitive: FCC `§15.247` / EU duty citations in code are real; keep them.

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.

## Git workflow

- The monorepo is developed straight on `main`. **Do not create new Git branches** (or open
  PRs) unless the user explicitly asks; work on the current branch and commit there.
- Commit and push only when the user requests it.
