# tower-cli

Host CLI/TUI for the **HARDWARIO TOWER** console link (`cli/` in the
[hardwario/tower](https://github.com/hardwario/tower) monorepo). The `tower` binary connects to
a TOWER device over the USB serial port and renders the **framed** host↔target protocol
(COBS + CRC + postcard) emitted by the [TOWER firmware SDK](../firmware): logs, events, and an
interactive shell. (A plain serial monitor shows that link as raw frame bytes; `tower` decodes
it.) It also flashes the device over its UART bootloader and bridges a Radio Dongle's network
to MQTT.

The wire format is shared with the firmware via the [`tower-protocol`](../crates/protocol)
crate, a path dependency both build from the same source, so the two cannot drift at build
time. A `tower` binary built before a wire change still speaks the old version: after pulling
a protocol change, rebuild it (see *Install*); a stale binary reports a protocol-version
mismatch (exit `125`) rather than mis-decoding.

## Install

From a checkout of the repository (the usual way while developing firmware), at the repo root:

```sh
cargo install --locked --path cli      # or: just install-cli
```

Or grab the archive for your platform from a `cli-vX.Y.Z`
[release](https://github.com/hardwario/tower/releases), extract it, and put `tower` on your
`PATH`:

```sh
# macOS / Linux
tar -xzf tower-cli-vX.Y.Z-<target>.tar.gz
sudo mv tower-cli-vX.Y.Z-<target>/tower /usr/local/bin/

# verify the download against its checksum and its build-provenance attestation
shasum -a 256 -c tower-cli-vX.Y.Z-<target>.tar.gz.sha256
gh attestation verify tower-cli-vX.Y.Z-<target>.tar.gz --repo hardwario/tower
```

On Windows, extract the `.zip` and put `tower.exe` on your `PATH`. (Archives of the 1.x line,
released from the former tower-cli repository, are named `tower-vX.Y.Z-<target>`.)

| Platform | Target |
|----------|--------|
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| Linux ARMv7 (Raspberry Pi) | `armv7-unknown-linux-gnueabihf` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

## Usage

```sh
tower --help          # all commands
tower --version       # print the tower-cli version
tower                 # open the TUI console on the detected device (bare, no subcommand)
```

The streaming commands render the framed link to stdout:

```sh
tower logs                        # decoded logs + print! output
tower logs --color never          # force plain output (--color auto|always|never)
tower events                      # structured key=value events
tower exec "/system/resource print"   # run one shell command, print its reply, exit
tower exec "/slow/op" --timeout 5000   # widen the per-command idle timeout (ms)
tower logs --no-reconnect         # exit when the link drops instead of retrying
```

The interactive commands:

```sh
tower                             # bare: open the TUI console (same as `tower console`)
tower console                     # full-screen TUI: logs + events + shell in one view
tower console --reset             # NRST-pulse the device on connect to see it boot
tower shell                       # line-based interactive shell (commands start with /)
tower shell --reset --timeout 3000
tower complete "/sys"             # ask the target to complete a partial command line
```

And for transport debugging:

```sh
tower monitor                     # dump decoded frames as they arrive
tower monitor --hex               # dump every raw received byte as hex
```

Colors default to **auto**: on when stdout is a terminal and `NO_COLOR` is unset,
off when piped or redirected. The first device open is always fatal (a bad `--device`
exits non-zero rather than retrying); reconnection only kicks in after a successful
attach (an auto-detected device is re-detected on each attempt, so a replug that comes
back under a new name is followed), and `--no-reconnect` disables it entirely: the
command then exits `1` when the link drops.

The device is selected with `-d`/`--device` (auto-detected when exactly one USB serial
device is present). List what's connected with `tower devices`. On macOS each port appears
once, as its `/dev/cu.*` call-out node (the `/dev/tty.*` dial-in twin is hidden).

### Gateway & MQTT bridge

With a Radio Dongle running the `radio_dongle_gateway` firmware, `tower gateway` bridges
its radio network to MQTT: a full-screen TUI by default, or headless with `--service`. It
hosts an embedded MQTT broker unless pointed at an external one:

```sh
tower gateway                       # TUI + embedded broker on 127.0.0.1:1883 (anonymous)
tower gateway --service             # headless: MQTT is the only interface, logs to stderr
                                    #   (exits 1 if the bridge dies, for Restart=on-failure)
tower gateway --broker 0.0.0.0:1883 # embedded broker on every interface; needs a login:
                                    #   --mqtt-user (or TOWER_MQTT_USER) + TOWER_MQTT_PASSWORD
tower gateway --mqtt host:1883      # use an existing broker instead of hosting one
                                    #   (--mqtt-user + TOWER_MQTT_PASSWORD for auth)
tower gateway --prefix tower/       # MQTT topic prefix (default tower/)
tower gateway --reset               # NRST-pulse the dongle on attach (clears its RAM queue)
```

**Broker access.** The embedded broker on a **loopback** address (the default
`127.0.0.1:1883`) is **anonymous by design**: any local user or process on the machine can
control the gateway: list/rename/remove nodes, read node AES keys (`nodes show --keys`), run
remote shell commands, just as anyone who can open the dongle's serial port could. A
**non-loopback** bind (any other address, including `0.0.0.0` / `::`) is refused unless a
login is configured (`--mqtt-user` or `TOWER_MQTT_USER`, plus the password; prefer
`TOWER_MQTT_PASSWORD`, argv is visible in `ps`); every client then needs the same login. A
configured login is enforced on loopback too. There is **no TLS**: the login and all traffic,
node keys included, cross the network in clear text; for remote access use an SSH tunnel or
VPN to a loopback broker, or bridge to a properly secured external broker with `--mqtt`. An
MQTT user without a password is an error; an exported `TOWER_MQTT_PASSWORD` without a user is
ignored. If the broker port is already taken, `tower gateway` fails instead of silently
talking to whatever owns it; with a wildcard bind its own client connects via loopback.

The MQTT topic tree (under `--prefix`) is the gateway's public API; the full table lives
in [`src/gateway/topics.rs`](src/gateway/topics.rs). Rules for clients writing to it:
commands (`gateway/cmd`, `nodes/{addr}/shell/req`) must not be retained (a retained one is
ignored and cleared); request ids must be 1-64 characters of `[A-Za-z0-9_.-]` (a uuid), and a
QoS-1 redelivery of a recent id is dropped; RPC responses arrive on `gateway/rsp/{id}`, so
subscribe once to `gateway/rsp/+` and match the id. Only nodes in the gateway's registry
are ever published; at startup the gateway drops downlinks a previous `tower gateway` left
queued on the dongle (their replies could not be matched).

The **client** commands drive a running gateway over MQTT (they share `--mqtt`/`--prefix`;
`<node>` is a `0xHHHHHHHH` address or a friendly name):

```sh
tower nodes list                                   # the gateway's registered nodes
tower nodes show <node> [--keys]                   # one node (--keys reveals its AES key)
tower nodes add --ota [--window 60] [--name N]     # pair over the air (node held in join mode)
tower nodes add --port <SERIAL> [--name N]         # pair a node over its USB cable
tower nodes rename <node> <name>
tower nodes remove <node>
tower nodes shell <node> "<line>" [--timeout MS] [--no-wait]   # remote shell, queued until wake
tower nodes pending <node>                         # queued, not-yet-delivered commands
tower nodes dequeue <node> <ref>                   # drop a queued command by ref
tower net status                                   # gateway online/offline + stats summary
```

Role is enforced: `tower gateway` at a node, or `nodes add --port` at a gateway, exits `126`.

### Exit codes

`tower` follows a small, stable exit-code contract so scripts and CI can branch on
the cause of a failure:

| Code | Meaning |
|------|---------|
| `0`  | success |
| `1`  | tool error: I/O (incl. a port that fails mid-command, or a `logs`/`events` link lost under `--no-reconnect`), bad file, encode/decode, truncated/incomplete device response; also `flash`/`erase` refusing a readout-protected board, and `protect`/`unprotect` not confirmed |
| `2`  | usage error (bad arguments, emitted by clap) |
| `124`| device command timed out (no response at all to `exec` / `complete`; also: no gateway reply to an MQTT RPC) |
| `125`| protocol mismatch: the device was built from a different tree (another `PROTOCOL_VERSION`, or the same version with a different wire fingerprint); rebuild `tower` or reflash the device from the same tree |
| `126`| wrong firmware role (`tower gateway`/`nodes add --port` pointed at the wrong kind of device) |
| `1..=123` | `exec` only: the device-reported shell result (clamped into this range) |

A response that arrives but is missing chunks (or runs past 256 KiB without ending) exits
`1`, not `124`: the device *did* answer, the output just can't be trusted. Likewise a port
that errors out (unplugged mid-command) is `1`: `124` strictly means a live link on which
the device stayed silent. `125` is deliberately distinct so CI can
detect the version-mismatch failure mode (stale `tower` binary vs. freshly flashed firmware)
instead of misreading it as a timeout.

### Protocol mismatch detection

Two checks catch a device built against a different `tower-protocol` than this `tower`:

- **`PROTOCOL_VERSION`** rides in every frame header; a frame of another version is rejected
  before its payload is read.
- **The wire fingerprint** (`tower-protocol` v1.6.0+, wire v4): the device's `Hello` (and its
  `Describe` reply, for `tower gateway` and `nodes add --port`) carries `WIRE_FINGERPRINT`, a
  compile-time hash of the whole wire schema. A device whose fingerprint differs from this
  build's speaks the **same `PROTOCOL_VERSION` but a different wire schema** (the two were
  built from different source trees, or someone changed the schema without bumping
  the version), which postcard would otherwise decode into silently wrong values.

Either one is handled the same way: the mismatch banner on stderr (naming both versions, or
both fingerprints in hex), exit code `125` for `exec`, `shell --reset`, `complete`, `tower
gateway` and `nodes add --port`, and no further interpretation of the stream: `logs`/`events`
and the TUI count the frames as undecodable instead of rendering them, until a matching
`Hello` arrives. The `hello:` line `tower logs` prints shows the device's fingerprint
(`protocol v4 fp 0x…`), and `/system/resource print` shows it on the device side (`wire:`).

## Flashing firmware

`tower` can program the device's STM32L0 over its UART bootloader (toggling
NRST/BOOT0 through the USB-UART bridge), so a separate flashing tool isn't
needed. The bootloader engine is the [`tower-boot`](../crates/boot) library
(formerly the `jolt` flasher, whose commands live here now):

```sh
tower flash firmware.bin     # erase, write, verify, then reset into the app
tower flash firmware.bin --no-verify --no-run
tower erase                  # erase the whole flash, reset into the app
tower reset                  # reset into the application
tower reset --bootloader     # reset into the system bootloader
tower chip                   # chip id + family, readout protection, bootloader version, commands
tower protect                # production lock: readout protection (RDP level 1), asks first
tower unprotect              # remove it: ERASES flash + data EEPROM, asks for "erase" first
```

`tower chip` is read-only (the former `jolt info`, plus the protection state): it enters the
bootloader, reads `Get ID` and `Get`, probes the readout protection with one Read Memory, then
resets back into the application on every path. The data goes to stdout, status lines to
stderr; `-v` prints the bootloader connect diagnostics (as for `flash` / `erase`):

```
Device  : /dev/ttyUSB0 @ 115200 baud, 8E1
Chip id : 0x447 (STM32L07x/L08x (e.g. STM32L083CZ))
Readout : not protected                  (or: Readout : protected (RDP level 1))
BL ver  : 3.1
Commands: 0x00 0x01 0x02 0x11 0x21 0x31 0x44 0x63 0x73 0x82 0x92
```

The `Commands` line is what the bootloader implements, the same at every protection level. The
former `jolt monitor` raw serial monitor has no counterpart: `tower monitor --hex` dumps every
received byte.

Firmware must be a raw `.bin` (convert `.elf`/`.hex` with
`arm-none-eabi-objcopy -O binary in.elf out.bin`). The device is auto-detected when
exactly one USB serial device is present; otherwise pass `--device`.

### Readout protection (production units)

Dev boards stay **open**. For a production unit, `tower protect` sets the STM32L0's **readout
protection to level 1**: from then on neither the UART bootloader nor a debugger (SWD) can read,
write or erase the flash (the firmware) or the data EEPROM (radio keys, pairing, settings, TX
counters), while the application keeps running from flash. `tower flash` and `tower erase`
refuse such a board up front:

```
[tower] error: this board is readout-protected; `tower unprotect` erases it (flash + EEPROM) so it can be re-flashed; it must then be re-paired
```

(exit `1`, the tool-error code; the message is the way to tell it apart). Level 2, which is
permanent (no SWD, no bootloader, no way back), is never set by this tool, and it has no
option-byte writes at all.

The way back, `tower unprotect`, **mass-erases the whole flash AND the data EEPROM** (that is
what the chip does when leaving level 1; RM0367 3.4.1): the node loses its firmware, radio keys,
pairing, settings and TX counters, and must be re-flashed and re-paired. On a board that is not
protected it does nothing (and erases nothing).

Both commands explain what they do and ask first: `tower protect` wants `yes`, `tower unprotect`
wants the word `erase`. `--yes` skips the question (for a scripted production line); without it,
a stdin that is not a terminal is refused before the port is touched. Afterwards they read the
state back in a new bootloader session and print it as the `Readout :` line (stdout), like
`tower chip`.

**Production provisioning** (flash, verify, pair, then lock):

```sh
tower flash app.bin                  # erase, write, read-back verify, start
tower exec "/system/resource print"  # the unit's functional check
tower nodes add --port <SERIAL>      # pair it with the gateway (or: --ota)
tower protect                        # RDP level 1, read back: "Readout : protected (RDP level 1)"
# power-cycle the unit once (unplug USB): after RDP level 1 is set through the bootloader the
# STM32L0 runs the application only after a power-down (RM0367 3.4.1)
tower chip                           # optional: "Readout : protected (RDP level 1)"
```

**Field re-flash** of a protected unit:

```sh
tower unprotect                      # type "erase": flash + data EEPROM wiped, "Readout : not protected"
tower flash app.bin                  # re-flash
tower nodes add --port <SERIAL>      # re-pair (new key; the default `auto` address is
                                     #   UID-derived and comes back, custom settings do not)
tower protect                        # lock it again before it goes back out
```

If `protect` reports that the bootloader acknowledged but the read-back still finds the chip
open, a debugger was most likely attached since power-on (the option bytes then load only on a
power-on reset, RM0367 3.4): power-cycle the board (USB and any SWD probe) and run `tower chip`.
If the application does not come back after a bootloader session on a protected board,
power-cycle it as well.

## Build from source

The crate is a member of the repository's host workspace; the toolchain is pinned by the
repo-root `rust-toolchain.toml` (rustup installs it). On Linux the `serialport` dependency
needs libudev:

```sh
sudo apt-get install -y libudev-dev pkg-config   # Debian/Ubuntu
cargo build --release -p tower-cli               # at the repo root; binary at target/release/tower
cargo test -p tower-cli                          # no hardware needed
```

## License

MIT © 2026 HARDWARIO a.s. (see the repo-root `LICENSE`).
