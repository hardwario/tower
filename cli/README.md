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
time. A `tower` binary built before a wire change still speaks the old schema: after pulling
a protocol change, rebuild it (see *Install*); a stale binary reports a schema mismatch
(exit `125`) rather than mis-decoding.

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
tower gateway --broker 0.0.0.0:8883 --broker-tls-identity broker.p12
                                    # embedded broker on every interface, over TLS; needs a
                                    #   login: --mqtt-user (or TOWER_MQTT_USER) +
                                    #   TOWER_MQTT_PASSWORD, and TOWER_BROKER_TLS_PASSWORD
tower gateway --mqtt host:1883      # use an existing broker instead of hosting one
                                    #   (--mqtt-user + TOWER_MQTT_PASSWORD for auth)
tower gateway --mqtt mqtts://host   # the same over TLS (port 8883; --mqtt-ca for a private CA)
tower gateway --prefix tower/       # MQTT topic prefix (default tower/)
tower gateway --reset               # NRST-pulse the dongle on attach (clears its RAM queue)
```

**Broker access.** The embedded broker's access policy depends on where it listens
(`--broker ADDR:PORT`; default `127.0.0.1:1883`, or `127.0.0.1:8883` with TLS):

| Bind address | Login | TLS | Result |
|--------------|-------|-----|--------|
| loopback (`127.0.0.0/8`, `::1`) | none | either | **anonymous**, by design |
| loopback | set | either | login enforced |
| anything else (incl. `0.0.0.0` / `::`) | none | either | refused |
| anything else | set | on | login enforced, over TLS |
| anything else | set | off | refused; with `--broker-allow-plaintext`: served, with a warning |

*Login* is `--mqtt-user` (or `TOWER_MQTT_USER`) plus the password, preferably from
`TOWER_MQTT_PASSWORD` (argv is visible in `ps`); every client then needs the same login. *TLS*
is `--broker-tls-identity` (below). On **loopback** any local user or process on the machine
can control the gateway: list/rename/remove nodes, read node AES keys (`nodes show --keys`),
run remote shell commands, just as anyone who can open the dongle's serial port could; TLS
does not change that (it protects the network path, and loopback has none). Off loopback,
the login and all traffic, node keys included, would otherwise cross the network in clear
text, so TLS is required too; `--broker-allow-plaintext` accepts clear text explicitly (for a
segment you trust) and says so at startup. Alternatively keep the broker on loopback and
reach it through an SSH tunnel or VPN, or bridge to an external broker with `--mqtt`. An
MQTT user without a password is an error; an exported `TOWER_MQTT_PASSWORD` without a user is
ignored. If the broker port is already taken, `tower gateway` fails instead of silently
talking to whatever owns it; with a wildcard bind its own client connects via loopback.

#### MQTT over TLS

**Clients** (`tower gateway --mqtt`, `tower nodes`, `tower net`) use TLS with
`--mqtt mqtts://HOST[:PORT]` (default port 8883), or `--mqtt-tls` with a plain `HOST[:PORT]`:

| Flag / env | Meaning |
|------------|---------|
| `--mqtt mqtts://HOST[:PORT]`, `--mqtt-tls` | MQTT over TLS (default port 8883); `mqtt://` or no scheme is plain MQTT (1883) |
| `--mqtt-ca <PEM>` | trust this CA (a PEM file; may hold several certificates) on top of the OS roots |
| `--mqtt-client-identity <P12>` | client certificate + key for mutual TLS (PKCS#12) |
| `TOWER_MQTT_CLIENT_IDENTITY_PASSWORD` | its password (environment only, never a flag) |

The OS root store is always trusted, so a broker with a public certificate needs no flags.
The broker's certificate must name the host given to `--mqtt` (a DNS name or an IP address in
its subject alternative names): the check is always on, and there is no switch to turn it
off; for a self-signed broker, pass its CA with `--mqtt-ca`. A TLS failure is reported with
what to check (untrusted certificate, name mismatch, a plain port, a missing client
certificate), also while `tower gateway` is still waiting for its first connect. A login
sent over plain MQTT to a host other than `localhost` / a loopback address gets a one-line
warning (not a refusal: the broker may sit on a segment you trust).

**The embedded broker** serves TLS with `--broker-tls-identity <P12>`: a PKCS#12 file holding
the server certificate (with its chain) and private key, its password in
`TOWER_BROKER_TLS_PASSWORD`. The file is checked at startup, and the broker reads it again
for every connection it accepts: keep it in place while the gateway runs (replacing it, with
the same password, rotates the certificate for new connections). The listener then speaks
TLS only. The bridge's own MQTT client does not use it: it connects to a second, plain
listener on `127.0.0.1` at a free port (announced at startup, same login), so the
certificate does not have to cover `localhost` and the bridge keeps its link even when the
TLS listener is busy (see below). Two limits of the embedded broker's TLS (rumqttd's
native-tls backend): it does not request client certificates (authentication is the login;
for mutual TLS use an external broker), and it runs each TLS handshake inside its accept
loop, so a peer that opens the port and never completes a handshake holds off new
connections until it disconnects (sessions already open, and the bridge itself, keep
working). On a network you don't control, firewall the port to the clients you expect, or
put a hardened broker in front (`--mqtt`).

A self-signed setup with OpenSSL 3 (a private CA, and a broker certificate for every name
and address clients will dial; keep it at 825 days or less, macOS refuses longer-lived TLS
server certificates even from a private CA):

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
  -keyout ca.key -out ca.pem -subj "/CN=TOWER gateway CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout broker.key -out broker.csr -subj "/CN=gateway.local"
cat > broker.ext <<'EXT'
subjectAltName = DNS:gateway.local, IP:192.168.1.5
extendedKeyUsage = serverAuth
keyUsage = critical, digitalSignature
basicConstraints = critical, CA:FALSE
EXT
openssl x509 -req -in broker.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 825 \
  -extfile broker.ext -out broker.pem
openssl pkcs12 -export -inkey broker.key -in broker.pem -certfile ca.pem -out broker.p12
                                        # asks for the export password
```

Then, on the gateway host and on a client machine:

```sh
export TOWER_MQTT_USER=ops TOWER_MQTT_PASSWORD='...'    # the login the broker enforces
export TOWER_BROKER_TLS_PASSWORD='...'                  # the broker.p12 export password
tower gateway --service --broker 0.0.0.0:8883 --broker-tls-identity broker.p12

tower net status --mqtt mqtts://gateway.local --mqtt-ca ca.pem   # with the same login exported
```

Keep `ca.key` and `broker.key` private (`broker.p12` contains the key too); clients only need
`ca.pem`.

The MQTT topic tree (under `--prefix`) is the gateway's public API; the full table lives
in [`src/gateway/topics.rs`](src/gateway/topics.rs). Rules for clients writing to it:
commands (`gateway/cmd`, `nodes/{addr}/shell/req`) must not be retained (a retained one is
ignored and cleared); request ids must be 1-64 characters of `[A-Za-z0-9_.-]` (a uuid), and a
QoS-1 redelivery of a recent id is dropped; RPC responses arrive on `gateway/rsp/{id}`, so
subscribe once to `gateway/rsp/+` and match the id. Only nodes in the gateway's registry
are ever published; at startup the gateway drops downlinks a previous `tower gateway` left
queued on the dongle (their replies could not be matched).

The **client** commands drive a running gateway over MQTT (they share `--mqtt`, the login,
the TLS flags and `--prefix`; `<node>` is a `0xHHHHHHHH` address or a friendly name):

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
| `125`| protocol mismatch: the device was built from another checkout (its frames carry another schema tag); rebuild `tower` and the firmware from the same checkout |
| `126`| wrong firmware role (`tower gateway`/`nodes add --port` pointed at the wrong kind of device) |
| `1..=123` | `exec` only: the device-reported shell result (clamped into this range) |

A response that arrives but is missing chunks (or runs past 256 KiB without ending) exits
`1`, not `124`: the device *did* answer, the output just can't be trusted. Likewise a port
that errors out (unplugged mid-command) is `1`: `124` strictly means a live link on which
the device stayed silent. `125` is deliberately distinct so CI can
detect the schema-mismatch failure mode (stale `tower` binary vs. freshly flashed firmware)
instead of misreading it as a timeout.

### Protocol mismatch detection

Every console frame carries a 16-bit **schema tag** (`CONSOLE_TAG`) in its header, derived at
compile time from the whole console schema (every message type with its payload, the management
records); every radio envelope carries `RADIO_TAG` the same way. A device built from another
checkout than this `tower` puts another tag in every frame, so each one is refused before its
payload is read (postcard would otherwise decode it into silently wrong values). The tag is a
hash: it can't say which side is older, so the advice is always to rebuild both from the same
checkout (`just install-cli`, then reflash).

A mismatch is handled the same way everywhere: the banner on stderr (both tags, and the
device's firmware name and version, read best effort from its `Hello`), exit code `125` for
`exec`, `shell --reset`, `complete`, `tower gateway` and `nodes add --port`, and no
interpretation of the device's frames: `logs`/`events` and the TUI count them as undecodable
until frames with this build's tag arrive (the device reflashed from this checkout). The
`hello:` line `tower logs` prints shows the schema tag, `/system/resource print` shows both tags
on the device side (`wire: console 0x… radio 0x…`), and `tower net status` the bridge's.

A change of the **framing** itself (header layout, COBS, CRC) is not covered by the tags: such a
device's frames all fail their CRC. When many frames in a row fail their CRC and none passes,
`tower` says so once: `every frame fails its CRC: likely a different framing (an older or newer
checkout); rebuild both`.

## Flashing firmware

`tower` can program the device's STM32L0 over its UART bootloader (toggling
NRST/BOOT0 through the USB-UART bridge), so a separate flashing tool isn't
needed. The bootloader engine is the [`tower-flasher`](../crates/flasher) library:

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

`tower chip` is read-only: it enters the
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

The `Commands` line is what the bootloader implements, the same at every protection level.
`tower monitor --hex` dumps every received byte.

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
needs libudev, and OpenSSL (the TLS stack of the MQTT links there) is built from source and
linked statically, which needs perl, make and a C compiler (macOS and Windows use the OS TLS
stack):

```sh
sudo apt-get install -y libudev-dev pkg-config perl make gcc   # Debian/Ubuntu
cargo build --release -p tower-cli               # at the repo root; binary at target/release/tower
cargo test -p tower-cli                          # no hardware needed
```

## License

MIT © 2026 HARDWARIO a.s. (see the repo-root `LICENSE`).
