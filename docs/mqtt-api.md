# TOWER gateway: the MQTT API

`tower gateway` bridges a Radio Dongle running the `radio_dongle_gateway` firmware to MQTT. Its
topic tree is the public API of a TOWER radio network: telemetry and node state flow out of it,
management requests and remote shell lines flow into it. This page is the reference for
integrators (Node-RED, Home Assistant, custom backends) who talk to the gateway over a broker
and never read its source. The `tower nodes` and `tower net` commands are clients of exactly
this API.

For the product (the push-button node, the Radio Dongle, pairing, the downlink queue) see
[`firmware/docs/gateway.md`](../firmware/docs/gateway.md); for running the bridge and its
flags see the [CLI README](../cli/README.md) (*Gateway & MQTT bridge*); for the threat model
see [`security-model.md`](security-model.md).

## 1. Overview

**What is published.** The bridge mirrors the dongle's node registry into retained topics,
decodes every uplink of a registered node into telemetry (button and accelerometer events,
temperature), publishes the gateway's health, statistics and pairing state, streams radio
diagnostics, and serves management requests (RPC) and remote shell lines. Only nodes in the
gateway's registry are ever published; an uplink whose radio counter is not above the last one
published for that node is dropped (a replay after a dongle reboot), so per node `counter`
values only grow within one bridge run (a `node_add` re-key restarts them).

**Prefix.** Every topic lives under a prefix, `tower/` by default (`tower gateway --prefix`).
Surrounding slashes are trimmed and one trailing `/` is added (`tower` and `tower/` are the
same); an empty prefix puts the tree at the broker root; `+`, `#` and NUL are refused. Clients
must use the same prefix (`tower nodes --prefix`). Two gateways on one broker need two
prefixes, e.g. `tower/site-a/` and `tower/site-b/`. The topics below are written without the
prefix.

**Node addressing.** A node is addressed by its fixed 32-bit radio address, written `0x`
followed by exactly 8 lowercase hex digits (`printf "0x%08x"`): `nodes/0x0000ab12/...`. Names
are mutable metadata that live only in payloads; topics never contain them. The bridge also
accepts upper case (`0X0000AB12`) where it parses an address, but always publishes lower case,
so use lower case everywhere.

**QoS and retain.** The bridge publishes and subscribes with QoS 1 (at least once): a message
can arrive twice, so de-duplicate events on `(addr, counter)` and responses on their `id`.
State topics are retained (the latest value is what a new subscriber gets); events, responses
and diagnostics are not. Publish requests with QoS 1 and **never retained** (see 4.1).
On every (re)connect to the broker the bridge republishes its whole retained state
(`gateway/status`, `gateway/stats`, `gateway/pairing`, every `nodes/{addr}` and its
`shell/pending`). When the broker is slow or unreachable the bridge drops radio diagnostics
first and keeps state and events (the oldest held events are shed only after a long outage).

**Last will.** The bridge connects with a last will on `gateway/status`: retained, QoS 1,
payload `{"state":"offline","schema":3}`. The broker publishes it when the bridge's connection
drops (the process exits or is killed, or its link stays silent for one and a half keep-alive
periods; the bridge uses 15 s), so a subscriber sees a dead gateway without timing anything
itself. The embedded broker lives in the bridge process and goes away with it, so the last will
matters on an external broker.

**Schema version.** `gateway/status` carries `schema`, the version of this whole topic and
payload tree, currently **3**. It is bumped with any change a subscriber would notice: a topic
or key renamed, removed or retyped. Additions (a new key, a new topic) do not bump it, so ignore
keys you do not know. A client reads the retained `gateway/status` (the last will carries
`schema` too) and, if `schema` is not the version it was written for, refuses to interpret the
tree: stop, report it and update the integration (or the gateway host). There is no negotiation
and no compatibility mode; `tower net status` exits 125 on a mismatch. `schema` is unrelated to
the serial and radio schema tags shown in `wire`.

**Conventions.** Every payload is UTF-8 JSON except the empty payload that clears a retained
topic. `ts` is Unix time in seconds from the bridge host's clock at publish time. RSSI values
are dBm. Key order inside an object is not significant (RPC `data` objects come with their keys
sorted). Use a unique MQTT client id; the bridge connects as `tower-gateway-0x<addr>` and the
`tower` CLI as `tower-cli-<pid>`.

### Access policy and connecting

`tower gateway` either hosts an embedded broker (the default) or connects to an external one
(`--mqtt [mqtts://]HOST[:PORT]`). The **embedded broker's** access policy depends on where it
listens (`--broker ADDR:PORT`; default `127.0.0.1:1883`, or `127.0.0.1:8883` with TLS):

| Bind address | Login | TLS | Result |
|--------------|-------|-----|--------|
| loopback (`127.0.0.0/8`, `::1`) | none | either | anonymous, by design |
| loopback | set | either | login enforced |
| anything else (incl. `0.0.0.0`, `::`) | none | either | refused at startup |
| anything else | set | on | login enforced, over TLS |
| anything else | set | off | refused, unless `--broker-allow-plaintext` (then served, with a warning) |

The login is `--mqtt-user` (or `TOWER_MQTT_USER`) plus the password in `TOWER_MQTT_PASSWORD`; it
is the one account the broker accepts, and every client needs it. TLS is
`--broker-tls-identity broker.p12` (a PKCS#12 server identity, its password in
`TOWER_BROKER_TLS_PASSWORD`). The TLS port speaks TLS only (minimum TLS 1.2), the server
certificate must name the host clients dial, and client certificates are not requested
(authentication is the login). On an anonymous loopback broker any local user or process can
drive the gateway, read node keys and queue remote shell lines, just as anyone who can open the
dongle's serial port could.

The embedded broker speaks **MQTT 3.1.1 over TCP** (no MQTT 5, no WebSocket listener) and caps
packets at 64 KiB. With an **external broker**, its own authentication and ACLs apply; the
bridge logs in with `--mqtt-user` / `TOWER_MQTT_PASSWORD` and uses TLS with `mqtts://`
(`--mqtt-ca` adds a private CA, `--mqtt-client-identity` presents a client certificate). Anyone
who can publish on `gateway/cmd` or `nodes/+/shell/req` controls the network, and anyone who can
subscribe to `gateway/rsp/+` or `gateway/cmd` can see node keys (section 4.3), so restrict those
topics with the broker's ACLs.

Connecting with the Mosquitto clients:

```sh
# Embedded broker on loopback (anonymous)
mosquitto_sub -h 127.0.0.1 -t 'tower/#' -v

# Embedded broker served off loopback with TLS and a login, private CA
mosquitto_sub -h gateway.local -p 8883 --cafile ca.pem -u ops -P "$TOWER_MQTT_PASSWORD" \
  -t 'tower/#' -v
```

Dial a name the broker certificate holds (`gateway.local` above, or an IP address listed in its
subject alternative names). Making the CA and the broker identity is shown in the
[CLI README](../cli/README.md) (*MQTT over TLS*).

## 2. Topics

Under the prefix; "gateway" in the second column means the bridge publishes, "client" means
clients publish and the bridge subscribes. Everything is QoS 1.

| Topic | Published by | Retained | Payload |
|-------|--------------|----------|---------|
| `gateway/status` | gateway (+ last will) | yes | [Status](#gatewaystatus) |
| `gateway/stats` | gateway | yes | [Stats](#gatewaystats) |
| `gateway/pairing` | gateway | yes | [Pairing](#gatewaypairing) |
| `gateway/cmd` | client | **never** | [RPC request](#41-protocol) |
| `gateway/rsp/{id}` | gateway | no | [RPC response](#41-protocol) |
| `nodes/{addr}` | gateway | yes (empty = removed) | [Node](#nodesaddr) |
| `nodes/{addr}/event/button` | gateway | no | [Button event](#nodesaddreventbutton) |
| `nodes/{addr}/event/accel` | gateway | no | [Accelerometer event](#nodesaddreventaccel) |
| `nodes/{addr}/measure/temperature` | gateway | yes | [Temperature](#nodesaddrmeasuretemperature) |
| `nodes/{addr}/uplink` | gateway | no | [Raw uplink](#nodesaddruplink) (diagnostic) |
| `nodes/{addr}/shell/req` | client | **never** | [Shell request](#5-remote-shell) |
| `nodes/{addr}/shell/rsp` | gateway | no | [Shell response chunk](#5-remote-shell) |
| `nodes/{addr}/shell/pending` | gateway | yes | [Pending queue mirror](#nodesaddrshellpending) |
| `radio/rssi` | gateway | no | [Ambient RSSI](#radiorssi) (diagnostic) |
| `radio/rx` | gateway | no | [Received packet](#radiorx) (diagnostic) |
| `radio/tx` | gateway | no | [Transmit outcome](#radiotx) (diagnostic) |

The diagnostics (`radio/*`, `nodes/{addr}/uplink`) are high-rate and worthless once stale: they
are dropped first under broker back-pressure and not sent at all while the broker link is down.

## 3. Payloads published by the gateway

### `gateway/status`

Retained. Published when the bridge connects to the broker, when the serial link to the dongle
goes up or down, and when the dongle reboots or answers an identity probe.

| Field | Type | Meaning |
|-------|------|---------|
| `state` | string | `online` (the dongle's serial link is up), `degraded` (the bridge runs but has lost the dongle; nothing new arrives and requests fail at once), `offline` (the last will: the bridge is gone) |
| `schema` | number | Version of this API (section 1), currently `3` |
| `gateway` | string | The dongle's radio address, `0x%08x` |
| `firmware` | string | Firmware name the dongle reports (`radio_dongle_gateway`) |
| `firmware_version` | string | Its version string, e.g. `v0.1.0` |
| `session_id` | number | The dongle's console session id from the last boot announcement this bridge saw; `0` until one is seen (the one at attach is consumed by the startup probe). A change means the dongle rebooted and its RAM downlink queue was lost |
| `wire` | string | The bridge's serial and radio schema tags, `console 0xXXXX radio 0xXXXX` (diagnostic; firmware and `tower` from the same checkout show the same tags) |
| `serial_port` | string | Serial port the bridge uses |
| `serial_up` | bool | The serial link to the dongle is up |

```json
{"state":"online","schema":3,"gateway":"0x0000ab12","firmware":"radio_dongle_gateway","firmware_version":"v0.1.0","session_id":7,"wire":"console 0x1234 radio 0xabcd","serial_port":"/dev/ttyACM0","serial_up":true}
```

The last will carries only two keys, so parse every other field as optional:

```json
{"state":"offline","schema":3}
```

### `gateway/stats`

Retained, refreshed every 10 s and on every (re)connect. Counters are the bridge process's, not
the dongle's.

| Field | Type | Meaning |
|-------|------|---------|
| `uptime` | number | Seconds since `tower gateway` started |
| `nodes` | number | Registered nodes |
| `uplinks` | number | Uplinks published since the bridge started |
| `queued` | number | Remote shell commands this bridge has pending, all nodes (queued on the dongle, or delivered and awaiting the reply) |
| `rssi` | number or `null` | Last ambient channel RSSI sample, dBm; `null` before the first |
| `channel` | number or `null` | Radio channel of that sample; `null` before the first |

```json
{"uptime":3600,"nodes":2,"uplinks":42,"queued":1,"rssi":-98,"channel":3}
```
```json
{"uptime":3600,"nodes":2,"uplinks":42,"queued":1,"rssi":null,"channel":null}
```

### `gateway/pairing`

Retained. The OTA pairing window (section 6). Published on every change, every second while a
window is open, and on every (re)connect.

| Field | Type | Meaning |
|-------|------|---------|
| `state` | string | `open` (a window is open) or `idle` |
| `remaining` | number | Seconds until the window closes; only while `open` (it can sit at `0` for up to 15 s while the dongle finishes the window) |
| `joined` | string | Address of the node that joined; present only on the `idle` message that ends a window with a join (or reports a late join) |
| `sas` | string | The join's 6-digit short authentication string, zero-padded (`"004321"`); present with `joined` |

```json
{"state":"open","remaining":45}
```
```json
{"state":"idle"}
```
```json
{"state":"idle","joined":"0x0000ab12","sas":"004321"}
```

The retained join result stays until the next pairing state change or the bridge's next broker
(re)connect, which republishes `{"state":"idle"}`.

### `nodes/{addr}`

Retained, one per registered node; an **empty payload** (not JSON) clears it when the node is
removed. Republished on registry refreshes (bridge start, `node_list`, adds, renames, pairing,
a dongle reboot), on each of the node's `NodeInfo` heartbeats, when a remote command is
queued for it, and on every broker (re)connect. The same object is an element of the
`node_list` RPC answer.

| Field | Type | Meaning |
|-------|------|---------|
| `addr` | string | The node's radio address, `0x%08x` |
| `name` | string | Friendly name, at most 16 bytes; `""` while `unnamed` |
| `kind` | string | Device kind from the node's firmware name (`radio_push_button` is `push-button`); `""` until the node's first `NodeInfo` heartbeat since the bridge started |
| `sleeping` | bool | The node sleeps between uplinks (downlinks wait for its next uplink) |
| `unnamed` | bool | No name assigned yet; the bridge auto-names the node `<kind>:<n>` (e.g. `push-button:0`) on its first heartbeat |
| `last_seen` | number or `null` | Seconds since the node's last uplink as the dongle reported it at the last registry refresh, or `0` after an uplink the bridge has seen since; not aged between refreshes, so for liveness use the `ts` of the node's events or a fresh `node_list`. `null` = not heard since the dongle booted |
| `rssi` | number or `null` | RSSI of the last uplink, dBm; `null` = none yet |
| `uplinks` | number | Uplinks since the dongle booted |
| `queued` | number | Downlink items queued on the gateway for this node (at least the remote commands this bridge has pending) |

```json
{"addr":"0x0000ab12","name":"kitchen","kind":"push-button","sleeping":true,"unnamed":false,"last_seen":3,"rssi":-67,"uplinks":12,"queued":1}
```

On an external broker, a retained `nodes/{addr}` left by an earlier bridge run is not cleared
if that node disappeared while no bridge was running; `node_list` is the authoritative registry.

### `nodes/{addr}/event/button`

Not retained. One per button event the node reports (which kinds it reports is the node's
`press` / `release` / `click` / `hold` settings; by default `click` and `hold`).

| Field | Type | Meaning |
|-------|------|---------|
| `event` | string | `press`, `release`, `click` or `hold` |
| `count` | number | The node's running count of this event kind since it booted (RAM only: a drop means the node rebooted) |
| `counter` | number | The radio frame counter: grows per node, the de-duplication key for QoS 1 redeliveries |
| `ts` | number | Unix time, seconds |

```json
{"event":"click","count":42,"counter":1234,"ts":1700000000}
```

### `nodes/{addr}/event/accel`

Not retained. Accelerometer events.

| Field | Type | Meaning |
|-------|------|---------|
| `event` | string | `motion` or `orientation` |
| `face` | number | Die face up, `1` to `6`; `0` = unknown or moving |
| `counter` | number | Radio frame counter (as for buttons) |
| `ts` | number | Unix time, seconds |

```json
{"event":"orientation","face":3,"counter":7,"ts":1700000000}
```

### `nodes/{addr}/measure/temperature`

Retained (the last value). The push button measures every `therm-period` and sends on a change
of at least `therm-delta`, or at the latest with its heartbeat.

| Field | Type | Meaning |
|-------|------|---------|
| `celsius` | number | Temperature, degrees Celsius |
| `millic` | number | The same, integer milli-degrees Celsius (the exact value the node sent) |
| `ts` | number | Unix time, seconds |

```json
{"celsius":21.5,"millic":21500,"ts":1700000000}
```

### `nodes/{addr}/uplink`

Not retained, diagnostic. Every uplink of a registered node as received, before decoding
(including heartbeats and shell reply chunks).

| Field | Type | Meaning |
|-------|------|---------|
| `counter` | number | Radio frame counter |
| `rssi` | number | dBm |
| `lqi` | number | Link quality indicator from the radio |
| `len` | number | Application payload length, bytes |
| `hex` | string | The application payload, lowercase hex (radio schema tag followed by the encoded message) |
| `ts` | number | Unix time, seconds |

```json
{"counter":5,"rssi":-67,"lqi":30,"len":3,"hex":"aabbcc","ts":1700000000}
```

### `nodes/{addr}/shell/pending`

Retained. A JSON array mirroring the remote shell commands this bridge has accepted for the node
and not finished: queued on the dongle, or delivered and awaiting the node's reply. `[]` when
there are none; cleared (empty payload) when the node is removed. Republished on every change.

| Field | Type | Meaning |
|-------|------|---------|
| `ref` | number | The dongle's queue item id (from 1, per dongle boot): the `ref` of `queue_drop` and the `item` of `radio/tx` |
| `id` | string | The `id` of the shell request |
| `line` | string | The command line |

```json
[{"ref":9,"id":"u-1","line":"/led on"}]
```

### `radio/rssi`

Not retained, diagnostic. The dongle's ambient channel RSSI, sampled every `stats-period`
(500 ms by default; the `stats_config` RPC overrides it).

| Field | Type | Meaning |
|-------|------|---------|
| `dbm` | number | Ambient RSSI, dBm |
| `channel` | number | Radio channel |
| `ts` | number | Unix time, seconds |

```json
{"dbm":-98,"channel":0,"ts":1700000000}
```

### `radio/rx`

Not retained, diagnostic. One per published uplink.

| Field | Type | Meaning |
|-------|------|---------|
| `src` | string | Sender address, `0x%08x` |
| `rssi` | number | dBm |
| `lqi` | number | Link quality indicator |
| `len` | number | Application payload length, bytes |
| `ts` | number | Unix time, seconds |

```json
{"src":"0x0000ab12","rssi":-67,"lqi":30,"len":3,"ts":1700000000}
```

### `radio/tx`

Not retained, diagnostic. One per gateway transmission attempt.

| Field | Type | Meaning |
|-------|------|---------|
| `dest` | string | Destination node, `0x%08x` |
| `item` | number | Queue item id (the pending `ref`); `0` = not a queue item |
| `outcome` | string | `delivered` (the node acknowledged), `not-delivered` (no acknowledgement after all retransmissions), `busy` (radio busy), `duty-limited` (held back by the duty-cycle limit), `error` (radio or crypto error), `expired` (the item's TTL lapsed before delivery; never sent). A queue item stays queued after `not-delivered`, `busy` or `duty-limited` and is retried on the node's next uplink |
| `ack_rssi` | number or `null` | RSSI of the node's acknowledgement, dBm; `null` without one |
| `ts` | number | Unix time, seconds |

```json
{"dest":"0x0000ab12","item":9,"outcome":"delivered","ack_rssi":-42,"ts":1700000000}
```

## 4. RPC: management requests

### 4.1 Protocol

**Request.** Publish on `gateway/cmd`, QoS 1, **not retained**:

```json
{"id":"3f2b8c1e-4d5a-4b6c-8d7e-9f0a1b2c3d4e","op":"node_list","params":null}
```

| Field | Type | Meaning |
|-------|------|---------|
| `id` | string | Correlation id: 1 to 64 characters of `[A-Za-z0-9_.-]`, fresh for every request (a UUID is ideal) |
| `op` | string | The operation (4.2) |
| `params` | object | The operation's parameters; may be omitted or `null` when it takes none |

**Response.** Published on `gateway/rsp/{id}`, QoS 1, not retained:

| Field | Type | Meaning |
|-------|------|---------|
| `id` | string | The request's `id` |
| `ok` | bool | Success |
| `data` | any | The result, when the op returns one (absent otherwise) |
| `error` | string | Why it failed; present only when `ok` is `false` |

```json
{"id":"u1","ok":true,"data":{"band":0,"channel":0,"gateway":"0x0000ab12"}}
```
```json
{"id":"u1","ok":true}
```
```json
{"id":"u1","ok":false,"error":"not found"}
```

Rules:

- **Subscribe first, once.** Subscribe to `gateway/rsp/+` before publishing and keep that one
  subscription for the session, matching responses by `id` (the same filter carries other
  clients' responses). Do not subscribe per request: on the embedded broker every distinct
  filter costs memory that is never freed.
- **Never retain a request.** A request delivered live runs; a retained copy the broker replays
  later (at the bridge's next subscribe) is ignored and cleared from the broker. A retained
  request therefore runs at most once, and not at all if the bridge was not connected when it
  was published.
- **Request ids.** A request whose `id` breaks the rules is dropped without a response (there
  is no safe topic to answer on). The bridge remembers the last 64 ids (RPC and shell requests
  together) and drops a repeat as a QoS 1 redelivery, also without a response. Payloads over
  4096 bytes, malformed JSON and a request without `id` or `op` are dropped too (the bridge
  logs why).
- **One response.** Every accepted request gets exactly one response (unless the bridge stops
  or its broker link stays down long enough for the response to be shed). A redelivered response
  can arrive twice; keep the first.
- **Timeouts.** The bridge sends management requests to the dongle one at a time, in arrival
  order, and fails one the dongle leaves unanswered after about 8 s with
  `device did not answer (timeout)`. Wait at least 10 s for an ordinary op (the `tower` CLI waits
  5 s, `--timeout`), and the window plus 20 s for `node_add_ota`. No response at all means the
  bridge is not running against this broker and prefix (check `gateway/status`).
- **Node parameters.** Where an op takes `addr` for an existing node, it accepts the canonical
  address or the node's current name (exact match); `node_add` takes the address only.

Errors common to the ops (all but the last come from requests that reach the dongle):

| `error` | Cause |
|---------|-------|
| `gateway serial link is down` | The bridge has lost the dongle (`gateway/status` is `degraded`); answered at once |
| `device did not answer (timeout)` | The dongle did not answer in time |
| `operation not supported by this device` | The dongle refused the op (e.g. no hardware TRNG for pairing) |
| `invalid argument` | The dongle refused an argument |
| `not found` | No such node or queue item on the dongle |
| `capacity exhausted` | Registry full (16 nodes), or the downlink queue full |
| `busy (conflicting operation in flight)` | E.g. a pairing window is already open on the dongle |
| `device storage error` | The dongle's EEPROM write failed, or its registry belongs to another firmware build |
| `unknown op "<op>"` | The `op` is not one of 4.2 |

### 4.2 Operations

| `op` | `params` | `data` on success |
|------|----------|-------------------|
| [`describe`](#describe) | none | `{band, channel, gateway}` |
| [`node_list`](#node_list) | none | array of [Node](#nodesaddr) |
| [`node_add`](#node_add) (secret) | `addr`, `key`, `name`?, `sleeping`?, `tx_floor`?, `rx_floor`? | none |
| [`node_add_ota`](#node_add_ota) | `window`? | `{joined, sas}` or `{joined: null}` |
| [`pairing_cancel`](#pairing_cancel) | none | none |
| [`node_rename`](#node_rename) | `addr`, `name` | none |
| [`node_remove`](#node_remove) | `addr` | none |
| [`reveal_key`](#reveal_key) (secret) | `addr` | `{addr, key, rx_floor, tx_floor}` |
| [`queue_list`](#queue_list) | `addr`? | array of `{addr, age, ref, ttl}` |
| [`queue_drop`](#queue_drop) | `addr`, `ref`? | none |
| [`stats_config`](#stats_config) | `period_ms`? | none |

#### `describe`

The gateway's network parameters. No params.

| `data` field | Meaning |
|--------------|---------|
| `gateway` | The dongle's radio address |
| `band` | `0` = EU 868 MHz, `1` = US 915 MHz |
| `channel` | Radio channel, `0` to `2` |

```json
{"id":"u-d","op":"describe"}
```
```json
{"id":"u-d","ok":true,"data":{"band":0,"channel":0,"gateway":"0x0000ab12"}}
```

#### `node_list`

The registry, read from the dongle. No params. `data` is an array of [Node](#nodesaddr)
objects; the call also refreshes every retained `nodes/{addr}`.

```json
{"id":"u-l","op":"node_list"}
```
```json
{"id":"u-l","ok":true,"data":[{"addr":"0x0000ab12","kind":"push-button","last_seen":3,"name":"kitchen","queued":0,"rssi":-60,"sleeping":true,"unnamed":false,"uplinks":5}]}
```

#### `node_add`

**Carries key material.** Registers a node with a key: a key the caller minted and installed on
the node itself (what `tower nodes add --port` does over the node's USB cable), or a key moved
from another gateway with its counter floors (section 4.3). OTA pairing does not use it. An
already registered address is **overwritten** (a re-provision).

| Param | Type | Default | Meaning |
|-------|------|---------|---------|
| `addr` | string | required | The node's address, `0x%08x` (not a name) |
| `key` | string | required | The AES-128 key, 32 hex digits |
| `name` | string | `""` | Friendly name; empty = unnamed (auto-named on the first heartbeat) |
| `sleeping` | bool | `true` | The node sleeps between uplinks |
| `tx_floor` | number | `0` | The exporting gateway's `tx_floor` (a moved key); `0` for a fresh key |
| `rx_floor` | number | `0` | The exporting gateway's `rx_floor`; `0` for a fresh key |

Validation errors: `node_add needs addr + key (hex)`;
`node_add: tx_floor / rx_floor must be u32 numbers` (a floor that is not an integer in
`0..=4294967295`; `null` counts as `0`); the name rules of [`node_rename`](#node_rename). The
dongle answers `invalid argument` for the reserved addresses `0x00000000` and `0xffffffff`, its
own address, or `tx_floor` `4294967295` (a key whose counter history is unknown cannot move:
re-pair that node), `capacity exhausted` when 16 nodes are registered, and
`device storage error` when the raised counter floor or the record cannot be stored.

```json
{"id":"u-a","op":"node_add","params":{"addr":"0x0000ab12","key":"0123456789abcdef0001020304050607","name":"kitchen","tx_floor":3073,"rx_floor":42}}
```
```json
{"id":"u-a","ok":true}
```

#### `node_add_ota`

Opens the gateway's over-the-air pairing window; see section 6 for the whole flow. The response
is **delayed** until the window resolves.

| Param | Type | Default | Meaning |
|-------|------|---------|---------|
| `window` | number | `60` | Window length in seconds, clamped to `1..=600` (a value that is not a non-negative integer means the default) |

| Outcome | Response |
|---------|----------|
| A node joined | `ok: true`, `data: {"joined": "<addr>", "sas": "<6 digits>"}` |
| The window expired, or `pairing_cancel` closed it | `ok: true`, `data: {"joined": null}` (not an error) |
| No answer within the window plus 15 s | `ok: false`, `device did not answer (timeout)` (a join reported later still appears on `gateway/pairing`) |

Errors: `a pairing window is already open` (this bridge has one open), and from the dongle
`busy (conflicting operation in flight)`, `capacity exhausted` (registry full),
`operation not supported by this device` (no hardware TRNG), `device storage error` (a
registry written by another firmware build).

```json
{"id":"u-ota","op":"node_add_ota","params":{"window":60}}
```
```json
{"id":"u-ota","ok":true,"data":{"joined":"0x0000cd34","sas":"004321"}}
```

#### `pairing_cancel`

Closes an open pairing window. No params, no `data`. A pending `node_add_ota` then answers
`{"joined": null}` and `gateway/pairing` returns to `{"state":"idle"}`. Harmless without an
open window.

```json
{"id":"u-c","op":"pairing_cancel"}
```
```json
{"id":"u-c","ok":true}
```

#### `node_rename`

| Param | Type | Meaning |
|-------|------|---------|
| `addr` | string | The node (address or current name) |
| `name` | string | The new name |

Name rules (also for `node_add`): 1 to 16 bytes of UTF-8 (`name must be 1..=16 bytes`),
printable, without control or bidirectional override characters
(`name must be printable text (no control characters)`), not shaped like an address
(`name must not look like a node address (0xHHHHHHHH)`), and unique
(`name "<name>" is already used by <addr>`). Other errors: `unknown node`, `missing name`, and
the dongle's `not found`. Renaming clears `unnamed`.

```json
{"id":"u-r","op":"node_rename","params":{"addr":"0x0000ab12","name":"kitchen"}}
```
```json
{"id":"u-r","ok":true}
```

#### `node_remove`

Unpairs a node: the dongle forgets its key and drops its queued downlinks. The bridge clears
the retained `nodes/{addr}`, `nodes/{addr}/measure/temperature` and `nodes/{addr}/shell/pending`
(empty payloads), and every pending shell command of the node ends with the error
`node removed`. A removed node can pair again (section 6). Errors: `unknown node`, `not found`.

```json
{"id":"u-rm","op":"node_remove","params":{"addr":"kitchen"}}
```
```json
{"id":"u-rm","ok":true}
```

#### `reveal_key`

**Returns key material.** The only path that ever discloses a stored node key: the key with the
two counter floors that must travel with it (section 4.3).

| Param | Type | Meaning |
|-------|------|---------|
| `addr` | string | The node (address or current name) |

| `data` field | Meaning |
|--------------|---------|
| `addr` | The node |
| `key` | The AES-128 key, 32 lowercase hex digits |
| `tx_floor` | The gateway's TX counter floor: above every counter it has sent under any key; `4294967295` = its history is unknown and the key cannot move |
| `rx_floor` | The highest counter the gateway accepted from this node (`0` if never heard) |

Errors: `unknown node`, `not found`.

```json
{"id":"u-k","op":"reveal_key","params":{"addr":"0x0000ab12"}}
```
```json
{"id":"u-k","ok":true,"data":{"addr":"0x0000ab12","key":"0123456789abcdef0001020304050607","rx_floor":42,"tx_floor":3073}}
```

#### `queue_list`

The dongle's downlink queue (the items it still holds for delivery).

| Param | Type | Default | Meaning |
|-------|------|---------|---------|
| `addr` | string | all nodes | One node (address or current name); one that names no node is refused (`unknown node`) |

Each element: `addr` (the node), `ref` (the item id), `age` (seconds since it was queued),
`ttl` (its lifetime, seconds).

```json
{"id":"u-q","op":"queue_list"}
```
```json
{"id":"u-q","ok":true,"data":[{"addr":"0x0000ab12","age":12,"ref":9,"ttl":3600}]}
```

#### `queue_drop`

Drops queued downlinks of one node.

| Param | Type | Default | Meaning |
|-------|------|---------|---------|
| `addr` | string | required | The node (address or current name) |
| `ref` | number | the whole queue | One item, `1..=65535` (from `shell/pending` or `queue_list`) |

The remote shell commands it removes end at once with the error `dequeued` and leave
`shell/pending`. Without `ref`, every not yet delivered command of the node goes. A `ref` that
was already delivered is no longer on the dongle (the RPC answers `not found`), but the bridge
stops waiting for its reply. Errors: `unknown node`, `ref must be a queue item id (1..=65535)`,
`not found` (nothing matched on the dongle).

```json
{"id":"u-qd","op":"queue_drop","params":{"addr":"0x0000ab12","ref":9}}
```
```json
{"id":"u-qd","ok":true}
```

#### `stats_config`

Sets the dongle's ambient RSSI sampling period (the `radio/rssi` cadence). It overrides the
dongle's `stats-period` setting (500 ms by default) in RAM only, until the dongle reboots.

| Param | Type | Default | Meaning |
|-------|------|---------|---------|
| `period_ms` | number | `500` (the dongle's default) | Milliseconds, capped at `60000`; `0` stops sampling |

```json
{"id":"u-s","op":"stats_config","params":{"period_ms":5000}}
```
```json
{"id":"u-s","ok":true}
```

### 4.3 Key material and counter floors

`reveal_key` answers and `node_add` requests carry a node's AES-128 key in clear JSON. Treat
them as secrets: every subscriber of `gateway/rsp/+` sees a `reveal_key` answer and every
subscriber of `gateway/cmd` sees a `node_add` request, so use them only over loopback or TLS,
keep those topics away from untrusted clients (broker ACLs), and never log the payloads. (The
bridge's own TUI redacts them.)

The floors exist because the radio's AES-CCM nonce is built from the sender's address and its
frame counter, and the counter belongs to the device, not to the key. If a key moves to a
gateway whose counter restarted (a replacement dongle at the old address, or the same dongle
after its store was wiped), that gateway would repeat nonces the key has already used, which
breaks CCM silently. So a moved key carries two floors, and `node_add` hands them to the new
gateway: it first raises its own TX counter to `tx_floor`, durably, then stores the key and
starts the node's replay window at `rx_floor`, so frames recorded earlier cannot be replayed
into it. A freshly minted key carries `0` for both. The floors are a **snapshot**: read them
with `reveal_key` right before the move and let the old gateway send nothing more; a key read
earlier (a backup), or one with `tx_floor` `4294967295`, cannot safely move to a gateway at the
old address: re-pair those nodes instead. Details: [`firmware/docs/radio.md`](../firmware/docs/radio.md)
(*Network layer*, "A key's counter history moves with the key") and
[`security-model.md`](security-model.md) (4.1 and residual risk 10).

## 5. Remote shell

A remote shell line runs in the node's shell (the same commands as its USB console) when the
node next talks to the gateway: a sleeping node only listens right after one of its own
uplinks, so the line waits in the dongle's downlink queue until then.

**1. Subscribe** to `nodes/{addr}/shell/rsp` (and, if you want the queue state,
`nodes/{addr}/shell/pending`) before sending.

**2. Request.** Publish on `nodes/{addr}/shell/req` (canonical lowercase address in the topic),
QoS 1, **not retained**:

| Field | Type | Default | Meaning |
|-------|------|---------|---------|
| `id` | string | required | Correlation id, the same rules as an RPC `id` (4.1) and the same duplicate memory |
| `line` | string | required | The command line, at most 56 bytes (the radio MTU) |
| `ttl` | number | `0` | Queue lifetime in seconds, `1..=65535`; `0` or absent = 3600 (one hour) |

```json
{"id":"8b0f2c9a-1d2e-4c3b-9a8f-7e6d5c4b3a21","line":"/system settings set therm-period=30","ttl":1800}
```

**3. Queued.** The bridge checks the node and the line and enqueues it on the dongle. Success
publishes no response; instead `shell/pending` gains an entry (`{ref, id, line}`) and
`nodes/{addr}` shows the new `queued` count. A refusal ends the dialog at once with a terminal
response (below).

**4. Delivered.** On the node's next confirmed uplink (a button event, a temperature report,
or its `NodeInfo` heartbeat, at least every `heartbeat` seconds, 900 by default) the dongle's
acknowledgement tells the node to listen and the dongle sends the line; `radio/tx` reports the
outcome with `item` = `ref`. The queue delivers one item per uplink cycle; it holds 4 items in
all and 2 per node, in RAM, so a dongle reboot loses it.

**5. Response.** The node runs the line and streams its reply as chunks, each published on
`nodes/{addr}/shell/rsp`:

| Field | Type | Meaning |
|-------|------|---------|
| `id` | string | The request's `id` (match it: other clients' dialogs with the node share the topic) |
| `chunk` | number | Chunk index, from `0` |
| `text` | string | This chunk's output (at most 56 bytes); concatenate the chunks in order |
| `done` | bool | The last message of the dialog |
| `result` | number | The shell result, authoritative only when `done`: `0` ok, `1` not found, `2` bad argument, `3` storage error, `4` output truncated, `5` denied (a console-only command, refused over the radio); with `error`, non-zero |
| `truncated` | bool | A chunk was lost on the air (a gap in `chunk`): the output is incomplete. Sticky for the rest of the dialog |
| `error` | string | Present only on a terminal failure reported by the bridge instead of a node reply; `text` is then empty |

```json
{"id":"abc","chunk":0,"text":"ok","done":true,"result":0,"truncated":false}
```

When `done` arrives the entry leaves `shell/pending`.

Every accepted request ends with exactly one message carrying `done: true`, either the node's
last chunk or one of these terminal errors:

| `error` | When |
|---------|------|
| `unknown node` | The node is not registered |
| `line too long for the radio MTU (max 56 bytes)` | `line` over 56 bytes |
| a dongle error, e.g. `capacity exhausted` | The dongle refused the enqueue (queue full); `result` is the dongle's refusal code |
| `not queued: gateway serial link is down` / `not queued: device did not answer (timeout)` | The enqueue never reached the dongle or was not answered |
| `queue TTL expired before delivery` | The node did not wake within `ttl` |
| `queue TTL elapsed with no delivery report from the gateway` | Backstop, `ttl` plus 30 s without any report |
| `delivered but no reply from the node` | Delivered, then no reply within 30 s |
| `gateway rebooted; command lost` | The dongle rebooted; its queue is gone (send the line again) |
| `no longer queued on the gateway (lost while the host link was down)` | Found missing after the serial link came back |
| `dequeued` | Dropped with `queue_drop` |
| `node removed` | The node was removed |

```json
{"id":"8b0f2c9a-1d2e-4c3b-9a8f-7e6d5c4b3a21","chunk":0,"text":"","done":true,"result":1,"truncated":false,"error":"queue TTL expired before delivery"}
```

A request whose `id` is invalid or repeated, that does not parse (e.g. a `ttl` over 65535), or
whose topic does not hold a canonical address, gets no response. Remote commands that would orphan a node (`/system/eeprom wipe`, changing its
address setting) are refused with `result` `5`; `/system reboot` works. The queue may deliver a
line more than once; the node recognizes a re-delivery and does not run it again.

## 6. Pairing over MQTT

OTA pairing adds a new node with an X25519 key agreement: no key travels, and the result is
checked by comparing a 6-digit short authentication string (SAS) shown at both ends.

1. Subscribe to `gateway/pairing` and `gateway/rsp/+`.
2. Send `node_add_ota` with the window you need (default 60 s, at most 600 s) and wait up to the
   window plus 20 s for the response. `gateway/pairing` turns to `{"state":"open","remaining":N}`
   and counts down every second.
3. Put the node in join mode: on an unpaired push button, hold the button for at least 1 s. A
   node that is paired already (moving to this gateway, or one whose gateway forgot it) re-pairs
   with a 12 s hold; OTA pairing never re-keys an address the gateway still has registered, so
   `node_remove` it here first.
4. On a join, `gateway/pairing` becomes `{"state":"idle","joined":"0x0000cd34","sas":"004321"}`
   and the RPC answers `{"joined":"0x0000cd34","sas":"004321"}`. The node then appears on
   `nodes/{addr}` (`unnamed`), and the bridge names it `push-button:0` (or the next free index)
   when its first heartbeat arrives, a few seconds later. Rename it with `node_rename`.
5. If no node joined, `gateway/pairing` returns to `{"state":"idle"}` and the RPC answers
   `{"joined":null}`. `pairing_cancel` ends the window early the same way.

**Late joins.** The dongle can commit a join a few seconds after the window (an EEPROM
compaction during the registry write); the bridge waits the window plus 15 s. If the dongle
reports a join even later, after `node_add_ota` already answered with a timeout error, the
node is registered all the same and the join is published on `gateway/pairing` with `joined`
and `sas`. A robust integration therefore watches `gateway/pairing` for `joined` rather than
relying on the RPC answer alone.

**Comparing the SAS.** `sas` is a string of exactly 6 digits; keep it a string (leading zeros
matter). The node logs the same value on its USB console as `net: pairing: SAS 004321` (and in
its `JOINED gateway ... (SAS 004321, ...)` line; watch it with `tower -d <node port> logs`).
Show the gateway's value to the person pairing and have them compare: equal values rule out a
man in the middle (an active attacker has a 1 in 10^6 chance of matching); different values
mean someone sat in between, so `node_remove` the node at once and pair it again. A SAS nobody
compares detects nothing.

## 7. Worked examples

The examples use the default prefix and the embedded broker on loopback; for TLS add the flags
from section 1 (`-h <name> -p 8883 --cafile ca.pem -u <user> -P <password>`) to every command.

**Watch everything:**

```sh
mosquitto_sub -h 127.0.0.1 -t 'tower/#' -v
```

**Check the gateway and the schema** (the retained status arrives at once):

```sh
mosquitto_sub -h 127.0.0.1 -t tower/gateway/status -C 1 -W 5
# {"state":"online","schema":3,"gateway":"0x0000ab12",...}
```

**An RPC** (subscribe first, then publish the request; the response topic ends with the id):

```sh
id=$(uuidgen)
mosquitto_sub -h 127.0.0.1 -q 1 -t 'tower/gateway/rsp/+' -v -C 1 -W 15 &
sleep 0.5
mosquitto_pub -h 127.0.0.1 -q 1 -t tower/gateway/cmd \
  -m "{\"id\":\"$id\",\"op\":\"node_list\"}"
wait
# tower/gateway/rsp/<id> {"id":"<id>","ok":true,"data":[{"addr":"0x0000ab12",...}]}
```

With other clients active, read more than one message (`-C`) and pick the one with your `id`.

**A remote shell command** (the reply arrives when the node next wakes; Ctrl-C ends the
subscriber here and in the next example):

```sh
node=0x0000ab12
id=$(uuidgen)
mosquitto_sub -h 127.0.0.1 -q 1 -v \
  -t "tower/nodes/$node/shell/rsp" -t "tower/nodes/$node/shell/pending" &
sleep 0.5
mosquitto_pub -h 127.0.0.1 -q 1 -t "tower/nodes/$node/shell/req" \
  -m "{\"id\":\"$id\",\"line\":\"/system settings set therm-period=30\"}"
# tower/nodes/0x0000ab12/shell/pending [{"ref":3,"id":"<id>","line":"/system settings set therm-period=30"}]
# ... the node wakes ...
# tower/nodes/0x0000ab12/shell/rsp {"id":"<id>","chunk":0,"text":"...","done":true,"result":0,"truncated":false}
# tower/nodes/0x0000ab12/shell/pending []
```

**Pair a node over the air:**

```sh
mosquitto_sub -h 127.0.0.1 -q 1 -v -t tower/gateway/pairing -t 'tower/gateway/rsp/+' &
sleep 0.5
mosquitto_pub -h 127.0.0.1 -q 1 -t tower/gateway/cmd \
  -m "{\"id\":\"$(uuidgen)\",\"op\":\"node_add_ota\",\"params\":{\"window\":60}}"
# tower/gateway/pairing {"state":"open","remaining":60}
# ... hold the node's button ...
# tower/gateway/pairing {"state":"idle","joined":"0x0000cd34","sas":"004321"}
# tower/gateway/rsp/<id> {"id":"<id>","ok":true,"data":{"joined":"0x0000cd34","sas":"004321"}}
```

**Over TLS with a login** (the embedded broker served with `--broker 0.0.0.0:8883
--broker-tls-identity broker.p12` and a login):

```sh
mosquitto_pub -h gateway.local -p 8883 --cafile ca.pem -u ops -P "$TOWER_MQTT_PASSWORD" \
  -q 1 -t tower/gateway/cmd -m "{\"id\":\"$(uuidgen)\",\"op\":\"describe\"}"
```

**Home Assistant** (a temperature sensor, unavailable unless the gateway is online):

```yaml
mqtt:
  sensor:
    - name: "Kitchen temperature"
      state_topic: "tower/nodes/0x0000ab12/measure/temperature"
      value_template: "{{ value_json.celsius }}"
      unit_of_measurement: "°C"
      device_class: temperature
      availability:
        - topic: "tower/gateway/status"
          value_template: "{{ 'online' if value_json.state == 'online' else 'offline' }}"
```

**Node-RED**: an *mqtt in* node on `tower/nodes/+/event/#` with QoS 1 and a parsed JSON output
gives one message per event; the node address is the third topic level with the default prefix
(`msg.topic.split("/")[2]`), and `msg.payload.counter` de-duplicates redeliveries.
