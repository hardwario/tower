# tower-cli: rules for agents

The `tower` host CLI: it decodes the firmware's framed console (logs, events, the shell, the TUI);
flashes, erases, resets and identifies the chip over the UART bootloader, or over USB DFU on a Core
Module r1.x (through `crates/flasher`; `src/target.rs` picks the route and reboots a running r1
into DFU through its USB console); sets or clears the production readout protection (`tower
protect` / `unprotect`: RDP level 1 only, both confirmed interactively or by `--yes`; `unprotect`
mass-erases flash and data EEPROM); and is the gateway and MQTT host (`tower gateway`, `tower
nodes`, `tower net`; `src/gateway/`, `src/client/`). Its manual is
[docs/cli/cli.md](../docs/cli/cli.md), its MQTT API [docs/cli/mqtt-api.md](../docs/cli/mqtt-api.md).

## MQTT and TLS

- MQTT over TLS (`src/gateway/tls.rs`) uses the platform's TLS stack through native-tls (rumqttc
  `use-native-tls`). **Never enable the rustls features of rumqttc/rumqttd**: their rustls
  versions fail `cargo audit`.
- rumqttd is built without TLS: the embedded broker's TLS port is our own front end
  (`src/gateway/tls_front.rs`, the crate's only async code, a current-thread tokio runtime on its
  own thread), which handshakes under a deadline with capped slots and proxies to a plain rumqttd
  listener on loopback (rumqttd's own TLS listener handshakes inside its accept loop, so one
  stalled peer blocks every client). Every rumqttd listener (the one behind the front end, the
  bridge's own) enforces the same login.
- The embedded broker's access policy is the pure `mqtt::broker_access` (loopback anonymous,
  anything else login + TLS unless `--broker-allow-plaintext`); change it together with its test
  matrix, docs/cli/cli.md and docs/security-model.md.

## The wire

An installed `tower` is the one thing a build cannot keep in step: after a wire change, reinstall
it (`just install-cli`), or it reports a schema mismatch (a banner naming the device's firmware,
exit 125) against freshly flashed firmware. A stream in which every frame fails its CRC (a
framing difference the tags can't see) is reported once as such.

## Checks

From the repo root (`just check` runs every gate):

```sh
cargo clippy -p tower-cli --all-targets -- -D warnings
cargo test -p tower-cli          # unit tests + the assert_cmd tests in tests/cli.rs; no hardware
```

Linux needs `libudev-dev` + `pkg-config`, plus `perl` + `make` + a C compiler for the vendored
OpenSSL. The TLS tests mint a throwaway CA and identities at test time and talk only to
localhost. The tests never open a real serial port; don't flash or attach to a board unless the
user asks.
