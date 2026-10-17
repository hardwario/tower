# tower-cli: working notes for coding agents

The `tower` host CLI (`cli/` in the hardwario/tower monorepo; read the root `AGENTS.md` first):
decodes the framed serial link from the TOWER firmware (logs / events / shell / TUI) and
flashes/erases/resets/identifies the chip over the UART bootloader (`tower flash` / `erase` /
`reset` / `chip`, via the `tower-flasher` library in `crates/flasher`), and sets or
clears the opt-in production readout protection (`tower protect` / `unprotect`: RDP level 1
only, both confirmed interactively or by `--yes`; `unprotect` mass-erases flash + data EEPROM).
It is also the gateway/MQTT host: `tower gateway` bridges a Radio Dongle's network to MQTT
(embedded `rumqttd` broker or external, TUI + `--service`), and `tower nodes` / `tower net` are
the MQTT client commands (`src/gateway/`, `src/client/`). MQTT over TLS (`src/gateway/tls.rs`)
uses the platform's TLS stack through native-tls (rumqttc/rumqttd `use-native-tls`); never
enable their rustls features, whose rustls versions fail `cargo audit`. The embedded broker's
access policy is the pure `mqtt::broker_access` (loopback anonymous, anything else login + TLS
unless `--broker-allow-plaintext`); change it together with its test matrix, `README.md` and
`docs/security-model.md`.

## Shared crates (path dependencies)

`tower-protocol` (`crates/protocol`, the wire format) and `tower-flasher` (`crates/flasher`, the
bootloader engine) come from the root workspace (`tower-protocol.workspace = true`,
`tower-flasher.workspace = true`). The firmware and the HIL harness build the same
`crates/protocol` source, so the wire format cannot drift at build time; there is no tag to
bump and nothing to keep in step by hand. A protocol change is made together with its callers
here in the same commit (see `crates/protocol/AGENTS.md`, "Changing the wire").

What a build cannot fix is an installed binary: after a wire change, reinstall `tower`
(`just install-cli` at the repo root, = `cargo install --locked --path cli`), or it reports a
schema mismatch (`SchemaMismatch`: the device's frames carry another schema tag; a banner naming
the device's firmware, exit 125) against freshly flashed firmware. A stream in which every frame
fails its CRC (a framing difference the tags can't see) is reported once as such.

## Checks

From the repo root (`just check` runs every gate):

```sh
cargo fmt --all -- --check
cargo clippy -p tower-cli --all-targets -- -D warnings
cargo test -p tower-cli          # unit tests + the assert_cmd tests in tests/cli.rs; no hardware
```

Linux needs `libudev-dev` + `pkg-config`, plus `perl` + `make` + a C compiler for the vendored
OpenSSL. The TLS tests mint a throwaway CA and identities at test time and talk only to
localhost. The tests never open a real serial port; don't flash
or attach to a board unless the user asks.

## Releasing

A `cli-vX.Y.Z` tag releases the `tower` binary (`.github/workflows/release-cli.yml`): it gates on
the tag matching `version` in `cli/Cargo.toml`, the tagged commit being on `main` and CI being
green for it, then builds cross-platform archives (`tower-cli-vX.Y.Z-<target>`) with checksums and
signed build-provenance attestations. Bump the version, commit, then tag; push only when the user
asks.

## Writing style

No em or en dashes. This applies everywhere: code, comments, docs, UI texts and commit messages. Use a comma, a semicolon, a colon or parentheses, or split the sentence. A plain hyphen is fine.

## Git workflow

- Part of the hardwario/tower monorepo, developed straight on `main`; no feature branches or PRs
  unless the user asks.
- Commit, tag and push only when the user requests it.
