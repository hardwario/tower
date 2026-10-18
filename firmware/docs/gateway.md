# TOWER Gateway: the wireless product guide

> **Status:** shipped and HW-verified. The two product
> apps, `apps/radio_push_button.rs` (a sleeping sensor node) and
> `apps/radio_dongle_gateway.rs` (the network coordinator), are complete firmwares, not
> skeletons. The host side (`tower gateway`, `tower nodes`, `tower net`, MQTT) lives in the
> `tower` CLI (`cli/`); this guide is the **device** side and the wire contract between them.

This is the standalone reference for the TOWER gateway product: a wireless push-button node
that reports button/temperature/accelerometer events and sleeps between them, a Radio Dongle
that bridges the secured radio network to the host, and the schema they speak. For
the radio stack itself (framing, CCM, pairing handshake, FHSS, duty cycle) see
[`radio.md`](radio.md); for the console framing and the settings framework see
[`console.md`](console.md).

---

## Three pieces

```
   push-button node  ──radio (CCM, NodeMsg schema)──▶  gateway dongle  ──serial──▶  tower gateway  ──▶ MQTT
   apps/radio_push_button        transparent bridge       apps/radio_dongle_gateway       (cli/)          tower nodes / net
   (STM32L083, sleeps)           forwards Uplink verbatim  registry + queue + pairing      TUI | --service
```

1. **Node** (`radio_push_button`): a battery product. Button events (with per-kind counts),
   periodic temperature, accelerometer tilt events. STOPs between events (µA); the SPIRIT1
   sleeps. Downlinks reach it through the gateway's queue on its next uplink.
2. **Gateway** (`radio_dongle_gateway`): the USB Radio Dongle. A **transparent bridge**: it
   authenticates/decrypts each uplink at the net layer and forwards the application payload
   *verbatim* to the host; it never decodes the node's app schema. What it owns is
   *coordination*: the node registry, the downlink queue, pairing, and radio diagnostics.
3. **Host** (`tower gateway`): decodes the radio application schema, bridges to MQTT, and
   drives the gateway via the management channel. See the [CLI README](../../cli/README.md)
   (Gateway & MQTT) and, for the MQTT topics and payloads, the
   [MQTT API reference](../../docs/mqtt-api.md).

The transparent-bridge split is the design's backbone: **a new node app type needs no gateway
firmware change**, because the gateway never interprets `NodeMsg`/`NodeCmd`; only the node
firmware and the host CLI agree on that schema (tagged separately; see below).

---

## The gateway link

Four console message types carry the gateway link, on top of the log/event/shell set (full table
in [`console.md`](console.md)):

| Type | # | Dir | Payload |
|---|---|---|---|
| `MgmtResponse` | 7 | T→H | `req_id, result, chunk, last, data` (chunked reply, `ShellResponse`-style) |
| `Uplink` | 8 | T→H | `src, counter, rssi, lqi, data` (a node's radio payload, forwarded verbatim) |
| `RadioStat` | 9 | T→H | ambient channel-RSSI sample, or a per-TX delivery outcome |
| `MgmtRequest` | 18 | H→T | `req_id, op` (a `mgmt::MgmtOp`) |

Two **separately tagged** schemas ride these frames:

- **`tower_protocol::mgmt`**: the console-side management contract (host ↔ device), part of the
  console schema: covered by `CONSOLE_TAG` in every frame header. `MgmtRequest{req_id, op}` → one
  or more `MgmtResponse` chunks with the same `req_id`; the `data` chunks concatenate into a
  stream of postcard records typed per op.
- **`tower_protocol::radio`**: the application schema *inside* the encrypted radio frame, carried
  opaquely by `Uplink`/`QueuePush`. Tagged by its own `RADIO_TAG` (2 bytes, LE) at the head of
  every envelope, so a radio-only change leaves the console tag alone and the gateway never
  decodes it; the node and the host check it on every envelope (a node or host built from
  another checkout: `SchemaMismatch`, logged with both tags). Envelope:
  `RADIO_TAG ‖ postcard(NodeMsg | NodeCmd)`, always ≤ `MAX_RADIO_PAYLOAD` (74 B).

### Management ops (`mgmt::MgmtOp`)

| op | served by | reply record |
|---|---|---|
| `Describe` | both | `DeviceInfo` (role probe: the authoritative "is this a gateway?"; also the firmware name + version, for a host that missed the boot `Hello`) |
| `NodeList` | gateway | `NodeEntry` × N (chunked) |
| `NodeAdd {addr,key,name,flags,tx_floor,rx_floor}` | gateway | none (install a cable-paired node, or a moved key with its counter floors) |
| `NodeRemove {addr}` / `NodeUpdate {addr,name?,flags?}` | gateway | none |
| `NodeRevealKey {addr}` | gateway | `NodeKey` (the only path that discloses a stored key; with its counter floors) |
| `PairingOpen {window}` / `PairingCancel` | gateway | `Paired {addr, sas}` (delayed; `sas` = the handshake's 6-digit SAS) / `MGMT_TIMEOUT` |
| `QueuePush {node_addr,ttl,data}` | gateway | `QueueId` |
| `QueueList {node_addr}` / `QueueDrop {node_addr,item?}` | gateway | `QueueEntry` × N / none |
| `StatsConfig {channel_period_ms}` | gateway | none (RAM override of `stats-period`) |
| `Provision {addr?,gw_addr,key,band,channel}` | node | `ProvisionAck` |
| `JoinOpen {window}` | node | `Joined {gw_addr, sas}` (delayed, after the commit round) |

Result codes: `MGMT_OK`(0), `UNSUPPORTED`(1), `BAD_ARG`(2), `NOT_FOUND`(3), `FULL`(4),
`BUSY`(5), `STORAGE`(6), `TIMEOUT`(7). An op a device does not serve → `UNSUPPORTED`; a
concurrent pairing window → `BUSY`. **Keys:** a cable-provisioned key is minted by the host
(its CSPRNG) and travels host→device in `NodeAdd`/`Provision`; an OTA-paired link key is
**derived on both ends** by the X25519 key agreement (docs/radio.md "OTA pairing") from TRNG
ephemerals, so `PairingOpen` carries no key. Keys never travel device→host except through the
deliberate `NodeRevealKey`.

### Radio application schema (`radio::NodeMsg` / `NodeCmd`)

Node → host (`NodeMsg`): `Info(NodeInfo{firmware_name, firmware_version, session_id, sleeping})` ·
`Button{kind, count}` · `Temperature{millic}` · `Accel{kind, face}` ·
`Shell(NodeShellChunk{cmd_id, result, chunk, last, text})`. `ButtonKind` = Press/Release/Click/
Hold; `AccelKind` = Motion/Orientation. Per-kind button `count` is RAM-only; a reset means the
node rebooted (watch `NodeInfo::session_id`).

Host → node (`NodeCmd`): `Shell{epoch, cmd_id, line}`, queued as a downlink; the node runs it in
the standard shell dispatcher and answers with `NodeMsg::Shell` chunks (≤ `RADIO_SHELL_CHUNK` =
56 B) correlated by `cmd_id`. `epoch` is a random per-host-process tag: the queue is at-least-once,
so the node de-duplicates re-deliveries on `(epoch, cmd_id)`: the random epoch stops a restarted
host that reuses a low `cmd_id` from being mistaken for a re-delivery (the "empty checkmark" reply).

### TX outcomes (`RadioStat::Tx.outcome`)

`TX_DELIVERED`(0, node ACKed), `TX_NOT_DELIVERED`(1), `TX_BUSY`(2), `TX_DUTY_LIMITED`(3),
`TX_ERROR`(4), `TX_EXPIRED`(5, TTL lapsed before delivery; never transmitted).

---

## The push-button node (`radio_push_button`)

A sleeping node. Every enabled button event sends a secured `NodeMsg::Button` with that kind's
running count; temperature is measured on a period and sent on-change or at latest every
heartbeat; the accelerometer's tilt interrupt wakes the node for `NodeMsg::Accel` events. All
of it is remote-shell reconfigurable (`system settings set …`).

### Settings

| setting | kind | default | range / values | effect |
|---|---|---|---|---|
| `therm-period` | uint s | `60` | 5..86400 | temperature measure/send cadence |
| `therm-delta` | uint c°C | `50` | 0..10000 | send when |ΔT| ≥ this; `0` = every period |
| `accel` | enum | `medium` | off/low/medium/high | tilt sensitivity (**boot-applied**) |
| `heartbeat` | uint s | `900` | 60..86400 | max interval between uplinks (`NodeInfo`) |
| `press` | bool | `off` | on/off | master enable: Press events |
| `release` | bool | `off` | on/off | master enable: Release events |
| `click` | bool | `on` | on/off | master enable: Click events |
| `hold` | bool | `on` | on/off | master enable: Hold events |
| `debounce-press` | uint ms | `30` | 1..1000 | press-recognition debounce (**boot-applied**) |
| `debounce-release` | uint ms | `30` | 1..1000 | release debounce (**boot-applied**) |
| `click-timeout` | uint ms | `500` | 50..5000 | max press length still counted a click (**boot-applied**) |
| `hold-time` | uint ms | `1000` | 100..10000 | press length that becomes a hold (**boot-applied**) |
| `addr` | addr | `auto` | hex / auto / random | SDK base: the node's 32-bit radio address |
| `identity` | str | *(empty)* | ≤ 32 chars | SDK base: friendly device name |

**Event master enables gate the whole path.** A disabled event is ignored *entirely*: not
reported over radio and not shown on the LED. The default is the coherent **gesture** scheme:
`click` + `hold` on, `press` + `release` off (a click is a quick press+release, so reporting all
four is redundant and their LED shapes blend on a tap). Enable `press`/`release` for raw-edge
reporting.

**Timing is recognition, not indication.** `debounce-*`, `click-timeout`, and `hold-time`
define what the physical input *means*; the LED feedback is a separate fixed indicator (below).

### LED behaviour

- **Boot signature (every TOWER board):** 500 ms off → 2 s on → 500 ms off
  (`Board::boot_indicator`), then the app's own behaviour begins. A 3-second power-on
  fingerprint common to all firmwares.
- **Per-event feedback (fixed constants, not configurable):** Press = a single 250 ms pulse ·
  Release = 250 ms · Hold = 1000 ms · Click = a **double-blink** (2 × 100 ms). The click's
  distinct shape is deliberate: a quick tap fires Press on the down edge and Release+Click
  together on the up edge, so a shared shape would blend on the one LED. The LED reacts only to
  *enabled* events.
- **Pairing:** unpaired = a double-blink every 2 s (the background) for 3 minutes after boot,
  then dark (the cue costs LED current, and an unpaired node may sit in a drawer); any button
  press, or a management request on the cable, shows it again for 3 minutes. Pairing (the
  handshake and the commit round) = a fast blink; afterwards the background goes dark when
  paired, or back to the double-blink (for 3 minutes) when the attempt left the node unpaired.

### Radio duty and backoff

The temperature is measured every `therm-period` and sent when it changed or the heartbeat is
due. While uplinks go undelivered (the gateway switched off, out of range, the node removed),
the period doubles per consecutive miss up to `heartbeat`, so an orphaned node no longer spends
a confirmed uplink with every retransmission and ACK window on every period, forever. A delivery,
a button press or a new pairing ends the backoff. The accelerometer's interrupt line backs off
the same way (1 s doubling to 5 min) while it stays high after its source register was read (an
I²C error, a stuck line), instead of re-reading it in a loop with no await (which starved the
executor until the watchdog reset the node, a boot loop). A button held past `hold-time`, or
stuck, waits for the release edge instead of polling every 5 ms (the re-pair gesture's 12 s
clock runs in the main loop, from the Press, so it does not need the polling).

### `button simulate <ms>`

Injects a synthetic press of `<ms>` (1..15000) through the **real** recognition machine
(debounce → click/hold classification) and on into reporting, the console "finger" for testing
without a physical press. A `100` ms press is a click (> debounce, < click-timeout); a `1500` ms
press is a hold; on a paired node a `13000` ms press is the re-pair gesture. Used by the HIL
gateway test.

### Pairing (node side)

- **OTA**: hold the button ≥ 1 s while unprovisioned: the node pairs (X25519 key agreement
  with a commitment round, docs/radio.md "OTA pairing") with any gateway that has a window open.
  It logs `net: pairing: SAS 123456` at the end of the handshake; the same SAS on the gateway's
  side (its console, and `tower nodes add --ota`, which prints it) is the man-in-the-middle check
  (compared, it leaves an active attacker a 1 in 10⁶ chance; not compared, none is detected). With
  no gateway answering, a request goes out every 1.5 s (one TRNG ephemeral serves them all, so
  the button stays responsive); a whole handshake takes ~3.8 s. Without a working hardware TRNG the node refuses to pair (logged).
- **Commit round**: the new key is provisional (RAM only) until the gateway acknowledges an
  uplink under it: the node sends its `NodeInfo` confirmed (500 ms after the handshake, then up
  to 6 attempts 2 s apart, ~15 s in all) and only on an ACK persists `(gw_addr, key, band,
  channel)` in `NS_APP` and logs `JOINED gateway XXXXXXXX (SAS 123456, uid …)`. No ACK (a lost
  JOIN_CONFIRM, a full or failing gateway registry): it logs `pairing with gateway XXXXXXXX not
  confirmed (no ACK under the new key), discarded` and stays unpaired, so another hold retries
  (the gateway's window stays open after a lost confirm). Before this round a lost confirm left a
  node that believed it was paired while the gateway dropped all its uplinks, recoverable only
  over USB. The one asymmetry left is the reverse and needs every acknowledgement of the round to
  be lost: the gateway registered the node (it is in `tower nodes list`, never seen) while the
  node gave the key up; the gateway refuses that address's JOIN_REQ, so remove it there first.
- **Re-pair** (recovery): hold the button ≥ 12 s while paired (longer than any `hold-time`, so a
  normal Hold never becomes one): the node runs the same OTA pairing (fast blink). The current
  pairing stays in force, in RAM and in EEPROM, unless the new one commits, so an accidental long
  press changes nothing. It is the way back for a node whose gateway forgot it (a removed node, a
  wiped or replaced gateway) and the way to move a node to another gateway. OTA pairing never
  re-keys an address a gateway still has registered (see the gateway side), so: **remove the node
  on its gateway (`tower nodes remove <node>`), open a window (`tower nodes add --ota`), then hold
  the button 12 s.** A paired node whose uplinks go unacknowledged 8 times in a row logs a hint
  saying so (`gateway XXXXXXXX: 8 uplinks in a row unacknowledged; …`); it keeps sending, since
  the usual cause is range or a gateway that is off.
- **Cable**: on USB the node serves the management channel: `Describe` (role probe), `Provision`
  (host-minted credentials; the key never rides the shell history), and `JoinOpen`
  (host-initiated OTA join, commit round included: `Joined {gw_addr, sas}` means the gateway
  acknowledged the key; a join that does not commit answers `MGMT_TIMEOUT`). After a `Provision`
  the node reboots into its new identity; watch for the fresh `Hello`.
  `Provision` answers `BAD_ARG` for a reserved address (`0` / `0xFFFFFFFF`) as either end, or a
  `gw_addr` equal to the node's own (effective) address (a shared `src` would collide CCM nonces).
  The key must be minted for that provision (the CLI's cable pairing mints one every time): it
  rides the board's TX counter from where it stands, so an old key on a fresh board (a
  replacement pinned to the old node's address) would repeat that key's nonces.
- The node runs its net layer **registered peers only**: it accepts radio frames from its
  gateway and nothing else, sends to nobody else, and while pairing accepts host frames only if
  they are addressed to itself (and, after the first JOIN_RESP, only from that host). Its
  `NetConfig::key` is all-zero, not the link key; the commit round's `NodeInfo` goes to the
  gateway the handshake named, registered in RAM as a peer under the provisional key. A gateway
  the peer table refuses (a reserved address, or the node's own: its `addr` set to the
  gateway's) leaves the radio off with an error at boot, and discards a pairing before anything
  is sent under its key (`JoinOpen` answers `BAD_ARG`): nothing is ever sealed under another key
  with a colliding `src`.
- The node persists its gateway's replay lane on **every** accepted downlink (a handful per
  boot; the provisional peer of a commit round too), so a recorded downlink cannot be replayed
  after a reboot; a stored provisioning record
  with no TX watermark beside it boots TX-locked (docs/radio.md, *Counters, replay &
  persistence*).

---

## The gateway dongle (`radio_dongle_gateway`)

One main loop owns `Net` + the registry + the queue (no `&mut Net` shared across tasks) and
multiplexes: recv slices, management requests, the pairing window, and the stats tick.

**Registered nodes only.** The gateway runs its net layer with the default lane off
(`Net::set_default_lane(false)`): a frame from any address not in the registry/peer table (never
paired, or removed) is dropped on its clear header, before CCM and before any ACK, and never
reaches the host. Its `NetConfig::key` is derived from its own (cleartext) address and is **not a
secret**; with the default lane on, anyone could seal "uplinks" under it from an arbitrary address
(e.g. a removed node's). Re-transmits of a node's last confirmed frame are re-ACKed at most 9
times per counter, so a replayed capture can't drain the EU duty budget through ACKs.

**Replays across a gateway reboot.** The gateway persists each node's replay lane only every
32 accepted uplinks (EEPROM wear: its lanes see every uplink), so after a reboot it accepts up to
31 already-seen counters per node once more. Every `Uplink` frame carries the node's counter, and
the host (`tower gateway`) publishes each node's counters strictly increasing: a replayed uplink
at or below the highest it published is dropped and logged, not published as a new event. A
node's counter only grows (it jumps to its next reserved block on a node reboot); a `node_add`
re-key resets the host's record. The record is RAM-only: a host restarted together with the
dongle starts without one (docs/security-model.md, 4.1).

### Node registry (`tower_gateway::registry`, EEPROM)

Paired `(id, key, flags, name)` records, persisted in `NS_APP` postcard **buckets**, never
RAM-resident (bucket locals are ~270 B of transient stack), mirrored into the net peer table at
boot. `PER_BUCKET = 6`, `BUCKETS = 3` (18 slots ≥ capacity), **`CAPACITY = 16`** (matches the net
layer's `MAX_PEERS`), `MAX_NAME = 16`. Written only on add/remove/rename/pairing-commit, never
per-uplink (that would burn the part). Keys: `KEY_FORMAT = 0x00`, buckets from
`KEY_BUCKET_BASE = 0x10`. The format marker is stamped once, on a fresh store (not rewritten
on every boot); a marker of another value (firmware from another checkout) leaves the registry
untouched: it is not
interpreted, `NodeAdd` and `PairingOpen` answer `MGMT_STORAGE`, and no bucket is written. A
bucket that is present but cannot be read or decoded reads as empty (its nodes stop working,
the rest of the network does not) and is never written over: a new node goes to another bucket,
or the add answers `MGMT_STORAGE`.

### Downlink queue (`tower_gateway::queue`, RAM-only)

Opaque host-built `NodeCmd` envelopes awaiting a sleeping node's next uplink. Global pool
`QUEUE_CAP = 4`, per-node FIFO `PER_NODE_CAP = 2`, `MAX_ITEM = 74` B. Stable monotonic `u16` ids
(from 1; `0` reserved = "not a queue item" in `RadioStat::Tx`). TTL expiry drops stale items and
reports `TX_EXPIRED`. **The queue is RAM-only:** a gateway reboot drops it, made visible by the
`Hello` `session_id` bump; the host fails every pending command ("gateway rebooted; command
lost") and the client sends it again. Delivery: each confirmed uplink's ACK carries a
*pending* flag; when set, the node opens a short RX window and the gateway delivers one item per
uplink cycle.

### Settings

| setting | kind | default | range / values | effect |
|---|---|---|---|---|
| `stats-period` | uint ms | `500` | 0..60000 | ambient channel-RSSI cadence; `0` = off. `StatsConfig` overrides at runtime. The sample is the RSSI latched by the latest idle receive-slice timeout (no separate deaf measuring window); a tick with no fresh idle sample (every slice caught a frame) is skipped |
| `band` | enum | `eu868` | eu868 / us915 | radio band (**boot-applied**) |
| `channel` | uint | `0` | 0..2 | radio channel (**boot-applied**) |
| `keyed-join` | bool | `on` | on/off | answer keyed joins (E4) from registered nodes; off = refuse them (logged). Re-read every 2 s, no reboot needed |
| `addr` | addr | `auto` | hex / auto / random | SDK base: the coordinator's radio address |
| `identity` | str | *(empty)* | ≤ 32 chars | SDK base: friendly name |

### Pairing (gateway side)

- **OTA**: host sends `PairingOpen{window}` (no key: the link key is derived by the X25519 key
  agreement); the gateway fixes the window's end (`window` after the request's arrival),
  precomputes its first ephemeral and nonce (~1.2 s; no hardware TRNG → `UNSUPPORTED` and an
  error log), opens the radio window, and on a completed handshake (the commitment round, then a
  valid JOIN_CONFIRM under the derived key) logs `net: pairing: SAS 123456`, commits the registry
  entry (flags `UNNAMED` until the host auto-names it from the first `NodeInfo` via
  `NodeUpdate`), logs `paired node XXXXXXXX (SAS 123456)` and answers the *delayed*
  `Paired {addr, sas}`, or `MGMT_TIMEOUT` on expiry. The host prints the SAS (`tower nodes add
  --ota`, the TUI and service logs, the `gateway/pairing` topic): compare it with the node's to
  rule out a man in the middle (an attacker's chance of matching it is 1 in 10⁶). **OTA pairing
  only adds new nodes:** a JOIN_REQ from an address that is already registered (or reserved, or
  the gateway's own) gets no answer, and the gateway logs `pairing: JOIN_REQ from XXXXXXXX
  refused (already registered …)`. The key agreement is unauthenticated until the SAS is
  compared, so answering would let anyone re-key a registered node by joining *as* it. **To
  re-pair a known node (e.g. after a factory reset, or with its 12 s re-pair hold), `NodeRemove`
  it first**, then open the window. The registry commit is insert-only as a backstop
  (`registry::add_new`; an existing address answers `BAD_ARG`).
- **The window closes on the first commit**, and the very next loop turn serves receive slices
  again: the joined node's commit round (its first confirmed `NodeInfo` under the new key,
  docs/radio.md) is ACKed and forwarded like any uplink. While a window is open the gateway listens
  only for pairing frames (registered nodes' uplinks wait for the window to end; a node retries
  them like any unacknowledged uplink), which is why a window closes as soon as it served its
  purpose. A JOIN_REQ is answered only while a whole handshake (~3.9 s) still fits before the
  window's end (`pairing: JOIN_REQ from … refused (the window closes before a handshake could
  finish)`), so no node is committed after the host was told the window expired. A commit can
  still take a few seconds past the window (its registry write may absorb a synchronous EEPROM
  compaction, ~5.2 s); `tower gateway` waits the window plus 15 s, and reports a `Paired` that
  arrives even later (`node … paired after the window's answer timed out`, plus the
  `gateway/pairing` topic) instead of dropping it.
- **Watchdog**: the main loop checks in at its top, after the management requests and after each
  pairing slice, so no stretch between check-ins (the 10 s liveness gate) stacks a PairingOpen's
  ephemeral, a pairing slice's handshake and a commit's EEPROM compaction.
- **Cable**: the host (which alone can reach the node's serial port) provisions the node
  directly, then registers it on the gateway with `NodeAdd{addr, key, name, flags, tx_floor,
  rx_floor}` (both floors 0: the key is freshly minted). `NodeAdd` answers `BAD_ARG` for `0`,
  `0xFFFFFFFF` or the gateway's own address; for an already-registered address it **overwrites**
  the record (an authenticated, host-side re-provision). A node re-added under a **new** key starts
  a fresh replay lane (its counter may have restarted); under the same key it resumes its old one.
- **Moving keys** (a replacement dongle, or this one after a factory reset): `NodeRevealKey`
  answers the key with two counter floors, `tx_floor` (this gateway's durable TX watermark,
  above every counter it sent under any key) and `rx_floor` (the node's last-seen). A `NodeAdd`
  of that key elsewhere passes them back: the gateway first raises its own TX counter to
  `tx_floor`, durably (watermark written and read back; `MGMT_STORAGE` and nothing stored if that
  fails), then stores the key and starts the node's lane at `rx_floor`. Without that, a gateway at
  the same address whose counter restarted would repeat every `(key, gw_addr, counter)` nonce of
  the old one: a CCM break, invisible on the air, since a node does not check an ACK's own
  counter. The floors are a **snapshot**: read them from the live gateway when the keys move,
  and let it send nothing more under them; a key read earlier (a backup), or one exported with
  `tx_floor = 0xFFFFFFFF` (that gateway lost its watermark; `NodeAdd` answers `BAD_ARG`), cannot
  move to a gateway at the old address: re-pair those nodes. The CLI side: `tower nodes show
  <node> --keys --json`, then the `node_add` RPC with `tx_floor` / `rx_floor` (cli/README.md).
- **Keyed join (E4)**: a node registered with `NodeAdd` may also join on air under its own
  key (docs/radio.md "Keyed join"): the gateway answers it inside the normal receive slice, with
  no window and nothing to commit, and logs `node … joined (keyed)`. The node learns the
  gateway's address that way: a node provisioned by cable or in the factory finds its gateway
  without a pairing window, and a replacement gateway that imported the node keys (`NodeAdd` of
  each entry of the old registry, read with `NodeRevealKey`, with its floors: *Moving keys*
  above) is re-joined without re-pairing. The `keyed-join` setting (default
  `on`) is the operator's switch; a refused keyed join (switched off, unregistered address,
  wrong key, stale counter, ...) is logged as `net: keyed join from XXXXXXXX refused: <reason>`,
  at most one line per 10 s. A node re-provisioned with a new key (and a restarted counter)
  joins fine after its `NodeAdd`. A counter restarted under the unchanged key would show up as
  `stale counter`, but by then its frames have already reused nonces on air: that is why it must
  not happen (a fresh key per provision, the fail-closed boot, the floors), not a state to
  recover from by re-adding.

### RAM budget

flip-link splits the 20 KB SRAM into **statics at the top** (`.data`/`.bss`/`.uninit`) and the
**stack below** them, so a stack overflow faults below the RAM base instead of silently corrupting
`.bss`. Everything an app keeps *resident* eats the statics half and shrinks the stack half:

| App | Statics | Stack budget | Measured peak (SWD stack-paint, 2026-07-11) |
|---|---:|---:|---|
| `radio_push_button` | ~10.8 KB | ~9.4 KB | **~6.8 KB** (radio-send + button + KV; stable under compaction churn) |
| `radio_dongle_gateway` | ~11.4 KB | ~8.6 KB | ~3.3 KB console-idle; deep registry/mgmt paths not bench-driveable standalone, **est. ~7.5 KB** |

The statics are **~67 % async task futures**: Embassy stores each task's across-`await` state
resident, so `__embassy_main`'s future alone is ~4.3 KB (the app's own main-loop state, not
framework overhead). That's *why* the stack peak is only ~7 KB and not ~15: async trades a deep
transient stack for resident static state. It also means growing resident state (a bigger
`MAX_PEERS`, more buffers held across awaits) shrinks the stack directly, which is how an
early over-budget gateway build HardFaulted at boot (stack overflow; flip-link now makes that a
link/boot fault, not `.bss` corruption). Hence `MAX_PEERS` **16** (not 32), the registry on
EEPROM (transient bucket locals), the 4-item queue, and 12-byte packed link stats.

**Guard:** `just ram-budget` (and CI) fails if any product bin leaves less than an 8 KB stack.
It is a regression tripwire so a future-inflating change is caught at PR time, not at a bench HardFault.
Re-check `just size app radio_dongle_gateway` after growing anything resident; if a bin genuinely
needs more, raise the floor *and re-measure the high-water mark on hardware*, don't just nudge it.

**Measured high-water:** the boot painter (`stack::paint`) fills the free stack with a sentinel
and `system stack print` reports how much was overwritten. The HIL test
`gateway_stack_high_water_within_budget` drives the deepest console-reachable paths (fill the
registry across all EEPROM buckets → the ~270 B bucket-codec local + a KV flip, push queue items,
list the whole registry → mgmt chunking) and reads it back: **7360 B of 8960 B** on the bench
(2026-07-12), under the 8 KB floor with ~1600 B to spare, confirming the earlier ~7.5 KB estimate.
The radio-bridge path (`net.send`) is exercised by `gateway_bridges_push_button_end_to_end`.

---

## Remote shell over the radio

The full round-trip, node asleep between events:

1. Host encodes `NodeCmd::Shell{epoch, cmd_id, line}` (random per-process `epoch` for
   at-least-once de-dup) and `QueuePush`es it to the gateway.
2. The gateway holds it; the next uplink's ACK advertises the pending flag.
3. The node opens a short RX window, decodes the `NodeCmd`, and runs `line` through
   `shell::stream_line` (the same command tree the console uses, but streamed): it re-runs the
   handler once per output window so an unbounded reply (`export`, `settings print`) is never
   truncated at a fixed buffer. The reply streams back as `NodeMsg::Shell` chunks (each reply's
   ACK re-flags pending, chaining the queue).
4. The gateway forwards the chunks as `Uplink` frames and reports delivery via `RadioStat::Tx`.

No tab completion over the air; completion is a per-transport feature the radio transport
doesn't offer.

**Console-only commands.** A few commands are refused over the radio with result **5
(`R_DENIED`)** and run only from the node's USB console: `system eeprom wipe` (a factory reset
must be physical; it erases the pairing key) and `settings set` of an address setting (re-
addressing a live node over the air orphans it). `system reboot` still works remotely. Anyone
who can reach the gateway's MQTT broker can queue remote shell lines, so this is the backstop
that keeps a broker-level intruder from unpairing the fleet (see the access policy in the
[CLI README](../../cli/README.md)). App commands opt in with `Entry::cmd_console_only`; see
[console.md](console.md).

---

## Building, flashing, testing

```bash
just build app radio_push_button      # or radio_dongle_gateway
just run   app radio_push_button      # build + flash + open the console TUI
just size  app radio_dongle_gateway   # on-chip footprint (watch the RAM budget)
```

The end-to-end product path is covered by the HIL bench group `hil/tests/gateway.rs` (two boards,
`#[ignore]`d): cable-provision the node, bridge a simulated `button simulate 100` click, run a
remote shell command through the downlink queue, and assert the `RadioStat` stream. The group is
driven from the HIL harness (`hil/`) with `just hil` there (or `just hil hil` from the repo root);
it flashes real hardware.

## See also

- [`radio.md`](radio.md): the secured radio stack (CCM, confirmed delivery, pairing handshake,
  FHSS, duty cycle) this product rides on.
- [`console.md`](console.md): the console framing, the full MsgType table, and the settings
  framework (`Kind`, base settings, result codes).
- The [CLI README](../../cli/README.md) (*Gateway & MQTT bridge*): the host commands (`tower gateway`, `tower nodes`,
  `tower net`).
- The [MQTT API reference](../../docs/mqtt-api.md): the MQTT topic tree, its payloads and the RPC ops (the gateway's
  public API).
