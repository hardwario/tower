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
- **Two boards.** The default build is the Core Module r2 (FT231X bridge, console on USART1,
  `VBUS_SENSE` on PA12). Feature `core-module-r1` builds for the first Core Module (rev 1.3:
  native USB on PA11/PA12, no bridge): the console becomes a USB CDC-ACM port (`console::usb`,
  host found by a pulse of the USB D+ pull-up, reset emulation over DTR/RTS, `sysboot` for the
  ROM bootloader's USB DFU, which it enters by erasing the first page of each flash bank: the
  L07x/L08x bootloader returns to valid code otherwise); everything else is shared. `just ci` builds, lints, documents and RAM-checks both
  (`check-r1`, the r1 lines of `ram-budget` / `clippy` / `doc`); keep both green. Design and
  limits: `docs/console.md`, "Core Module rev 1.3 (native USB)".
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
("Changing the wire": change the types and fix the firmware, the CLI and the harness in the same
commit; nothing to bump). Then reflash the boards and reinstall the CLI (`just install-cli` at
the repo root): a `tower` binary from another checkout reports a schema mismatch (exit 125)
against the new firmware, because every frame carries the schema tag it was built with.

## Conventions

- `tower-protocol` was developed here (in `src/`) before it was extracted; design rationale +
  hard-won caveats live in the `docs/*.md` guides and in code comments (don't strip them).
- Radio is regulatory-sensitive: FCC `§15.247` / EU duty citations in code are real; keep them.

## Log messages

What the device logs is user interface (the TUI's log pane, `tower logs`, the bench), so every
line speaks in one voice; `docs/console.md` (*Logging*) shows the same rules to SDK users.

- **Target = the subject.** `target:` names what the line is about, short and lowercase: an
  SDK subsystem (`boot`, `crash`, `net`, `radio`, `rng`, `storage`, `watchdog`) or a part of the
  app (the push button logs `node`, `button` and `accel`; the gateway `gateway`). The console
  shows `target: message`, so the message never repeats the target. A message may open with a
  sub-subject and a colon when a subsystem logs several kinds of things (`net: pairing: SAS
  123456`).
- **Lowercase, no trailing period.** Words keep their case only when they have one of their own:
  acronyms and part names (ACK, SAS, TRNG, EEPROM, I2C, IWDG, USB, TMP112). No capitals for
  emphasis or state (`PAIRED`, `JOIN`; the verdict words below are the one exception), no
  decorative characters (an ellipsis, arrows, dashes, `***`); unit symbols such as `°C` are fine.
- **The event or the state first, then the facts**, short (aim under 72 characters; the TUI's log
  pane is narrow): `click #3`, `paired with gateway 0x0000ab12, SAS 123456`, `pairing window
  open, 60 s`.
- **Values look the same everywhere:** radio addresses `0x%08x` (as `tower` and the MQTT API
  print them), a unit after a space (`60 s`, `-71 dBm`), an event's sequence number `#n`, a
  SAS as six digits, a count of repeats as `(n since the last report)`.
- **Context in parentheses, briefly:** `(not paired)`, `(pairing)`. A remedy only where the
  user has to act, once, on the line that reports the state, not repeated on every event that
  state affects.
- **Protocol messages by name in words** (`join request`, `reveal`, `confirm`), not their wire
  identifiers.
- **One line per event; a long operation logs its start and its outcome**, not each step.
- **Levels:** `error` when a function is lost (no radio, storage failed, a crash); `warn` when
  something failed or was refused and the device carries on (an undelivered uplink, a refused
  frame, a backoff); `info` for state changes and the events a user acts on; `debug` for
  developer detail. A warning that can repeat is rate-limited and reports its count.
- **Verdicts.** A self-checking example (a known-answer test, an edge test, a bench tool) states
  each result as a verdict: the last word of its line (only context in parentheses may follow),
  one of the capital tokens `PASS`, `FAIL`, `MATCH` (a value equals its reference) and
  `MISMATCH`, the only capital words a message may hold apart from acronyms and part names:
  `aes: FIPS-197 vector MATCH (AES-128 ECB)`, `edge: RX timeout recovery FAIL (7 of 10 back in
  ready)`. It ends with one summary verdict, `all checks PASS` (the phrase the bench waits for)
  or `some checks FAIL`: `edge: frame limits, all checks PASS`. A `PASS` or `MATCH` line is
  `info`, a `FAIL` or `MISMATCH` line `error`.
- **Bursts.** The console queue holds about 1.5 KB of encoded messages (20 to 30 typical
  lines) and the logger cannot wait for room (`log` is synchronous), so a longer back-to-back
  burst loses its newest lines; the host then shows `N log frame(s) dropped`. Await
  `console::flush()` before a line that must arrive (a summary verdict always does) or between
  groups of lines.
- **Lines that tools read are an interface:** the verdicts, `net: pairing: SAS n` and the few
  example lines the bench parses (`pair: paired node 0x…, key …`, `persist: TX counter resumed at
  …`, `confirmed: #n delivered`) are matched by the HIL bench (`hil/tests`) and the hwtest
  scripts (`tools/hwtest`); change them together with their readers.

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.

## Git workflow

- The monorepo is developed straight on `main`. **Do not create new Git branches** (or open
  PRs) unless the user explicitly asks; work on the current branch and commit there.
- Commit and push only when the user requests it.
