# tower-cli

The `tower` binary: the host CLI and TUI of TOWER. It decodes a device's framed console (logs,
events, the shell), flashes, erases and resets boards, and bridges a Radio Dongle's network to MQTT
(`tower gateway`, `tower nodes`, `tower net`).

- [The manual](../docs/cli/cli.md): install, usage, the gateway and MQTT, flashing, exit codes.
- [The MQTT API](../docs/cli/mqtt-api.md) of `tower gateway`.

From the repo root: `cargo build --release -p tower-cli` (binary `target/release/tower`),
`cargo test -p tower-cli`, `just install-cli`.
