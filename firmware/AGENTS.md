# Firmware SDK (`tower-sdk`): rules for agents

The Embassy `no_std` SDK for the Core Module (STM32L083CZ): the library `tower` (package
`tower-sdk`, `use tower::...`), educational `examples/` (`--example`) and the TOWER IoT Kit apps
in `apps/` (`[[bin]]`, `--bin`). Its guides are in [`docs/firmware/`](../docs/firmware/): start
with [sdk.md](../docs/firmware/sdk.md); [docs/README.md](../docs/README.md) is the index.

- A new radio app goes on taf ([docs/firmware/taf.md](../docs/firmware/taf.md), which also holds
  the app contract every radio app keeps): declare its kind in `crates/protocol/src/props.rs`,
  write its task, nothing else.
- `lcd_games` keeps one file per game in `apps/lcd_games/`, pulled in with `#[path]`, so `just
  lists` still sees one `apps/*.rs`.
- Build and flash with `just`, kind then name (`just build app radio_button`, `just flash example
  blinky`, `just run example <name>`). Device control (logs, console, reset, erase, chip,
  devices) is the `tower` CLI's own job: call `tower` directly; there are no `just` wrappers for
  it.

## Workspace and builds

- `firmware/` is **its own Cargo workspace** (the root `Cargo.toml` excludes it), with its own
  `Cargo.lock` and size-tuned profiles. `.cargo/config.toml` here sets the default target
  (`thumbv6m-none-eabi`) and the linker (`flip-link`); cargo reads it from the CURRENT
  directory, so run cargo inside `firmware/` (or through `just` / `just fw <recipe>` from the
  repo root), never `cargo build --manifest-path firmware/Cargo.toml` from elsewhere (that
  silently builds for the host or links without flip-link).
- `just ci` (= CI's `firmware` job) runs every hardware-free firmware gate: fmt, the MCU build
  of lib + apps + examples, the RAM budget, clippy + the five role-feature variants, role
  builds, rustdoc, and the `examples`/`apps` list drift check. `just check` is the MCU build
  alone.
- **Two boards.** The default build is the Core Module r2 (FT231X bridge, console on USART1,
  `VBUS_SENSE` on PA12). Feature `core-module-r1` builds for the first Core Module (rev 1.3:
  native USB on PA11/PA12, no bridge): the console becomes a USB CDC-ACM port (`console::usb`,
  host found by a pulse of the USB D+ pull-up, reset emulation over DTR/RTS, `sysboot` for the
  ROM bootloader's USB DFU, which it enters by erasing the first page of each flash bank: the
  L07x/L08x bootloader returns to valid code otherwise); everything else is shared. `just ci`
  builds, lints, documents and RAM-checks both (`check-r1`, the r1 lines of `ram-budget` /
  `clippy` / `doc`); keep both green. Design and limits: docs/firmware/console.md, "Core Module
  rev 1.3 (native USB)".
- Host tests: the firmware itself is `no_std` and can't `cargo test`; its logic is tested
  through the shared crates it builds on. `just test` here (or `just fw test` from the repo
  root) runs those crates' host tests; `just test` at the repo root runs the whole host
  workspace. A UI is checked without looking at the module: the `lcd::SHELL` commands' `lcd dump`
  prints the frame, `tools/lcd_dump.py` renders it to a PNG, and `lcd press` drives the UI.
- The SDK builds the shared `no_std` crates by path (the root `AGENTS.md` table says what each
  holds; `tower-hmi` is re-exported as `tower::hmi`, `tower-strip` as `tower::strip`). A protocol
  change follows the root runbook (*Changing the wire format*) and `crates/protocol/AGENTS.md`.

## Log messages

What the device logs is user interface (the TUI's log pane, `tower logs`, the bench). Every line
follows [docs/firmware/console.md](../docs/firmware/console.md), *Writing log messages* (target =
subject, lowercase, the event first, values written alike, levels, verdicts, bursts). Two of those
rules break things when ignored:

- **Lines that tools read are an interface:** the verdicts, `net: pairing: SAS n` and the few
  example lines the bench parses (`pair: paired node 0x…, key …`, `persist: TX counter resumed at
  …`, `confirmed: #n delivered`) are matched by the HIL bench (`hil/tests`) and the hwtest
  scripts (`firmware/tools/hwtest`); change them together with their readers.
- **Bursts:** the console queue holds about 1.5 KB of encoded messages (20 to 30 lines) and `log`
  cannot wait for room, so a longer burst loses its newest lines. Await `console::flush()` before
  a line that must arrive (a summary verdict always does).
