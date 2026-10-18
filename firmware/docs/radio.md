# TOWER Radio: user guide

A sub-GHz radio stack for the TOWER Core Module, built on the **SPIRIT1**
transceiver (in the SPSGRF module). This is the standalone reference for using and
maintaining the stack: API, wire protocol, the design rationale behind the RF and
security parameters, and a worked example per feature.

> **Status: complete and hardware-verified.** The full stack is implemented and
> tested on two boards: the radio layer (SPI, power states, RF config, CW, TX/RX
> with quality metrics, CSMA/CCA, SLEEP/SHUTDOWN), on-MCU **AES-128-CCM**, the
> frame codec, and the network layer (confirmed delivery + ACK/retransmit,
> replay protection + counter persistence, the EU duty governor, bulk pull,
> OTA pairing, and per-peer keys for star / P2P). Pairing v3 (X25519 with a commitment round)
> is host-tested and its crypto KAT-verified on the board; its on-air test (`net_pairing`, the
> gateway product) is pending. A semi-fuzzy soak (`radio_interop`)
> exercises it all under randomized traffic with latched invariant checks.

## Hardware

SPIRIT1 ↔ STM32L083CZ wiring (from the board, see `src/board.rs`):

| Signal | Pin | Notes |
|---|---|---|
| SDN | PB7 | drive low to enable (hardware pull-up → boots in shutdown) |
| SPI CS | PA15 | software CS, ≥2 µs setup |
| SCLK / MOSI / MISO | PB3 / PB5 / PB4 | SPI1, mode 0, 4 MHz |
| nIRQ | PA7 | active-low, EXTI line 7 |

Crystal is **50 MHz** (not a TCXO). Band is runtime-selectable via the `Band` abstraction:
**EU 868** (ch0/1/2 = 868.1/868.3/868.5 MHz, 1 % duty) and **US 915** (ch0/1/2 =
915.0/915.2/915.4 MHz). 915 single-channel is bench-only; FCC §15.247 compliance comes from
the FHSS mode (see *Spectrum-access modes* below).

## Building & flashing

Examples live in `examples/`. Flash with the UART bootloader and watch the console:

```sh
just flash example <name>                 # build + flash (auto-detect port)
TOWER_DEVICE=/dev/cu.usbserial-XXXX just flash example <name>   # pick a board
tower logs                 # watch from boot

# two-board examples select a role via a Cargo feature:
TOWER_FEATURES=role-gateway just flash example <name>   # one board
TOWER_FEATURES=role-node    just flash example <name>   # the other
```

## Radio layer API (`tower::radio`)

The `Spirit1` handle owns the SPI bus, the SDN pin and the nIRQ line:

```rust
use tower::radio::{Spirit1, RfConfig, config};

let mut radio = Spirit1::new(
    b.radio_spi, b.radio_sck, b.radio_mosi, b.radio_miso,
    b.radio_cs, b.radio_sdn, b.radio_irq,
);
radio.exit_shutdown().await?;          // SDN low, wait for READY
radio.read_device_id()?;               // verify part 304 / version 48
config::apply(&mut radio, &RfConfig { band: config::Band::Eu868, channel: 0 }).await?;

// raw frames (≤96 B), nIRQ-driven, with signal quality:
radio.tx(&bytes, /*use_csma=*/ false, Duration::from_millis(200)).await?;
let (len, q) = radio.rx(&mut buf, Duration::from_secs(5)).await?;
//   q: rssi, rssi_raw, lqi (PQI), sqi, afc_raw

radio.cw_test(true).await?;            // unmodulated carrier (bring-up / range)
radio.to_standby().await?;             // low-power states: to_ready/to_standby/to_sleep
let raw = radio.rssi_sample().await?;  // on-demand channel RSSI  (dBm = raw/2 - 130), ~20 ms deaf
let idle = radio.take_idle_rssi();      // free: RSSI latched by the last idle rx() timeout

// Retune to another band/channel at runtime (one binary runs either band):
config::set_band(&mut radio, config::Band::Us915, 0).await?;   // raw layer
// or, on the network layer (also moves the duty policy): net.set_band(band, ch)
```

**Bands.** `Band::Eu868` (default) and `Band::Us915` both lie in the SPIRIT1 high
VCO band, so they share every setting except base frequency + channel spacing; the
band is a **runtime** choice: pass it to `Net::new`/`config::apply`, or switch a
live radio with [`Net::set_band`]/`config::set_band` (rewrites only the synth
registers; the VCO auto-recalibrates on the next TX/RX). **915 MHz is for bench
testing only**: this ~216 kHz narrowband signal is not FCC 15.247-compliant
(which needs FHSS or ≥500 kHz wideband). See `radio_band` for a live-switch demo.

RF configuration (`config::apply`) programs the 50 MHz-crystal-specific setup
(REFDIV, IF offsets, SYNT/WCP, GFSK 19.2 kbps, 20 kHz deviation, ~216 kHz RX
filter, sync `0xDB624715`, 16-bit CRC, PA ramp, AFC freeze-on-sync, CSMA timing)
plus the ST management work-arounds (per-mode SMPS, VCO current). VCO calibration
is left to the part's automatic per-channel calibration (more temperature-robust
than a one-time manual cal). See `src/radio/config.rs`.

## Security: AES-128-CCM (`tower::radio::{aes, ccm}`)

Cryptography runs on the **STM32L0 hardware AES engine** (keys never leave the
MCU). `aes::Aes` is a register-level ECB-block driver; `ccm::Ccm` builds
**AES-128-CCM** (N=13, L=2, 8-byte tag; confidentiality + integrity) in firmware:

```rust
use tower::radio::ccm::Ccm;
let mut ccm = Ccm::new();
let tag = ccm.seal(&key, &nonce13, aad, &mut data);     // encrypt in place
ccm.open(&key, &nonce13, aad, &mut data, &tag)?;        // verify + decrypt
```

Verified against FIPS-197 (AES ECB) and RFC 3610 Packet Vector #1 (CCM); see
`examples/crypto_aes_kat.rs` and `examples/crypto_ccm_kat.rs`.

## Wire protocol (`tower::radio::frame`)

Little-endian frame, fits the 96-byte FIFO:

```
| fmt_type | flags | src(4) | dest(4) | counter(4) | [bulk_idx(3)] | payload | tag(8) |
```

- first byte: bits[7:5] the frame format marker (`frame::FORMAT` = 1, fixed; another value is
  `FrameError::BadFormat`, dropped), bits[4:0] the frame type
  (`Data`, `Ack`, `BulkReq`, `BulkData`, `JoinReq`, `JoinResp`, `JoinConfirm`, `Beacon`,
  `JoinNonce`, `JoinReveal`).
- `flags`: `CONFIRMED`, `KEYED` (JOIN frames only), `LAST_CHUNK`, `BULK_ANNOUNCE`.
- The whole cleartext header is the CCM **AAD**; the payload is encrypted.
- **Nonce** (13 B, not transmitted, reconstructed from the header):
  `nonce_for(src, counter, bulk_index)` = `src ‖ counter ‖ bulk_index ‖ 0x0000`.
  `bulk_index` is 0 for non-bulk frames, so each (key, frame) nonce is unique.
- MTU: ≤ 74 B payload (non-bulk), ≤ 64 B per bulk chunk.
- **ACK payload** (`tower_net::ack`): `acked(4 LE) ‖ rssi(1, i8) ‖ flags(1)`, `ACK_LEN` = 6 B.
  Every ACK is exactly `ACK_LEN` bytes; a payload of any other length never resolves a wait.
  ACK flag bits (`ack_flags`):
  - bit 0 `PENDING`: the ACKer (a gateway) holds a queued downlink for the ACKed sender.
  - bits 1-7: unassigned; a receiver ignores unknown bits.

Build / open a secured frame:

```rust
use tower::radio::frame::{self, Header, FrameType, flags};
let hdr = Header { frame_type: FrameType::Data, flags: flags::CONFIRMED,
                   src, dest, counter, bulk_index: None };
let n = frame::seal_frame(&mut ccm, &key, &hdr, payload, &mut buf)?;   // → on-air bytes
let (hdr, range) = frame::open_frame(&mut ccm, &key, &mut buf[..n])?;  // verify + decrypt
```

Verified end-to-end (build → parse → open, tamper/wrong-key rejection, bulk index
in nonce) by `examples/crypto_frame_loopback.rs`.

## Network layer (`tower::radio::net`)

`Net` wraps one radio + the CCM engine + EEPROM-backed counters and serializes one
transfer at a time. It is the layer most applications use directly:

```rust
use tower::radio::net::{Net, NetConfig, SendResult};
use tower::radio::config::Band;

// `keys_stored: false`: a compiled-in key; an app that persists its keys passes `true`, so
// a store that lost its TX watermark fails closed instead of restarting the counter.
let mut net = Net::new(radio, b.kv, NetConfig {
    addr: 0x1111_1111, key: KEY, band: Band::Eu868, channel: 0, keys_stored: false,
}).await?;

// Confirmed send: TX the DATA frame, open a 200 ms ACK window, retransmit the
// byte-identical frame up to `reps` times on loss (random 0-100 ms backoff).
match net.send(GW_ID, b"hello", /*confirmed=*/ true, /*reps=*/ 3).await {
    SendResult::Delivered    => {}                   // ACKed (or unconfirmed & sent)
    SendResult::NotDelivered => {}                   // no ACK after all reps
    SendResult::Busy | SendResult::DutyLimited => {} // CSMA / airtime budget
    SendResult::WrongMode    => {}                   // plain send while AFA/FHSS owns the channel
    SendResult::NotSynced    => {}                   // FHSS: not locked to the hop schedule
    SendResult::Error(e)     => { let _ = e; }       // bad length, radio fault, or NonceLocked (fail-closed)
}

// Receive: authenticate, apply the counter/replay rule, auto-ACK a confirmed
// frame (caching it so a retransmit re-sends the identical ACK, no re-deliver).
if let Some(rx) = net.recv(Duration::from_secs(10)).await {
    let _ = (rx.src, rx.counter, rx.rssi, rx.confirmed, rx.data());
}
```

The auto-ACK cache holds only the **most recent** confirmed RX. In a busy star, a
confirmed frame from another node between a node's original send and its retransmit
evicts that node's cached ACK, so its retransmit isn't deduplicated and triggers a full
re-send (still correct: never a re-delivery or nonce reuse, just extra airtime).

**Counters, replay & persistence.** Every transfer consumes one
monotonic TX counter; the counter feeds the nonce, so it must never repeat. The
watermark is persisted *ahead* in blocks of `RESERVE=1024`, so after a reboot the
device resumes **at or above** the last value it could have sent, never reusing
one (at most one block is skipped per reboot). A receiver accepts only a strictly
higher counter than it has seen from that sender and lazy-persists the last-seen
every `P=32` accepts (replay window ≤ P across a receiver reboot), or on every accept after
`net.set_lane_persist_period(1)`: what a sleeping node does (the push-button), since it hears a
handful of downlinks per boot and a lazy lane would never be written at all (a recorded
`system reboot` downlink then replayed after every reboot). The gateway keeps `P`: its lanes see
every uplink, and its host drops a replayed uplink by counter instead (docs/gateway.md). CCM
verify happens *before* the replay comparison, so a forged frame can't poison the state.

Every TX path (DATA, ACK, FHSS beacon, bulk announce/session/request, JOIN frames) takes its
counter from **one allocator** (`TxCounter::alloc` in `tower-net`, driven by
`Net::alloc_counter`) that hands the value out *already consumed*, **before** the transfer
awaits anything. A future dropped mid-flight (a `select(send, Timer)` timing out, an abandoned
bulk) therefore can't leave its counter for the next transfer to seal different plaintext under
the same nonce (`fhss_send`, `afa_send` and bulk used to advance only after their awaits). If
the watermark record is ever missing on a store that shows *any* other network-layer record
(a peer lane, the default-lane last-seen, the FHSS epoch), or whose app says it holds stored keys
(`NetConfig::keys_stored`: the node's provisioning record, the gateway's non-empty registry), the
boot fails **closed** (TX locked) rather than resuming at 1. The keys are written once, early in
the EEPROM log, while the watermark and the lanes are rewritten often and sit late in it, so one
corrupt record can orphan every network record at once and leave only the keys: without the
app's word that store looked virgin. A receiver re-ACKs a retransmit of a peer's last confirmed
counter at most `MAX_REPS − 1 = 9` times (the most a legitimate sender can ask for), so replaying
a captured frame (no key needed) can't drain the duty budget or TX counters through ACKs.

**A key's counter history moves with the key.** The TX counter belongs to the device, the key does
not: a key a gateway exports (`NodeRevealKey`) and a gateway at the same address imports after its
counter restarted (a wiped store, a replacement dongle pinned to the old address) would repeat
every `(key, src, counter)` nonce of its earlier life. So the export carries two **floors**:
`Net::tx_floor()` (the durable watermark, above every counter the exporter sent under any key;
`u32::MAX` when its history is unknown after a lost watermark) and the node's last-seen. The
importer raises its counter to the TX floor **durably** before it stores or uses the key
(`Net::raise_tx_floor`, the `TxCounter::raise_to` kernel: the watermark is written and read back,
and the key is refused if that fails) and starts the node's lane at the other one
(`Net::raise_peer_floor`). A freshly minted key carries 0 for both. Floors are a snapshot: they
cover what was sent until the export, so they are read from the live gateway when its keys move.
A node key is never moved onto another node's counter: only the node knows its own history and
keys never leave a node, so every `Provision` carries a key minted for it.

**Peer table & topologies.** Keys are per-peer. `add_peer(peer_addr, &key)` binds
a peer address to its own AES key and its own replay lane; an unregistered peer falls
back to the `NetConfig::key` default lane (the single-link case). One table holds
up to `MAX_PEERS = 16`. `add_peer` refuses a reserved address (`0`, `0xFFFFFFFF`) and the
device's **own** address (the nonce is `src ‖ counter ‖ …` under a key shared by both
directions, so a peer at our address would collide nonces).

**Registered peers only.** `net.set_default_lane(false)` turns the default lane off: `recv`
drops a frame from any unregistered `src` on its clear header: before CCM, before any ACK,
without touching the default lane's EEPROM record, and `send` (likewise `afa_send`, `fhss_send`,
bulk) refuses an unregistered `dest` (`RadioError::NotRegistered`) before it takes a counter:
nothing is ever sealed under the default key, which is then no link key (the product node's is
all-zero, so a peer `add_peer` refused can never fall back to its link key with a colliding `src`).
**A gateway must run this way** (the product
gateway and the `radio_gateway` example do): a default key that is not secret (the product
derives it from the gateway's own, cleartext address) would otherwise let anyone seal frames
from an unregistered address (say, a removed node) that the gateway ACKs and forwards as
genuine uplinks. The default stays *on* so the single-link `net_*` examples work unchanged.

**Lane restore is key-checked.** A peer's lane record (`NS_NET` locals `0x10..=0x1F`, 12 B:
`addr ‖ last_seen ‖ KCV`, KCV = first 4 bytes of `AES_K(0¹²⁸)`) is restored on `add_peer` only
under the **same key**; a re-add under a new key (a re-paired or factory-reset node whose
counter restarted) starts fresh at 0 instead of having every frame dropped as a replay. A new
peer takes a never-written lane local before it evicts a removed peer's record, so remove A →
add B → re-add A keeps A's replay window. A lane record of any other length is invalid and
reads as absent. Replays dropped from a registered peer are logged (`net` target, rate-limited
to one line per 10 s).

Registering and inspecting peers:

```rust
net.add_peer(NODE_A, &KEY_A);          // star hub: each node under its own key
net.add_peer(NODE_B, &KEY_B);
net.peer_count();  net.remove_peer(NODE_A);  net.peer_last_seen(NODE_B);
```

- **Star** (≤16 nodes): the gateway registers every node; `recv` reads the clear
  `src`, picks that node's key, and tracks a separate last-seen per node. Each node
  ships with only its own key.
- **P2P** (≤8 peers): both ends `add_peer` the other under a shared link key and
  exchange confirmed messages in either direction.

**Bulk transfer / downlink pull.** Large blobs are *pulled*: the sender
announces `(length, session)`, the requester pulls each ≤64 B chunk with a
`BULK_REQ(index)` and reassembles. The sender reserves **two** TX counters (one for the
announce frame and a separate `session` for every chunk), so the announce's nonce can
never collide with chunk 0's; the `session` counter + 24-bit chunk index then keep each
chunk's nonce unique. The sender frees an idle session after 30 s.

```rust
net.bulk_serve(NODE_ID, &blob).await;              // sender (in-RAM slice)
let n = net.bulk_fetch(GW_ID, &mut out).await;     // requester → bytes received
```

The transfer is **streamed on both ends**: the slice calls above are thin
wrappers over `bulk_serve_from(dest, &mut source)` / `bulk_fetch_into(src, &mut
sink)`, which pull each chunk from a [`BulkSource`] and hand each chunk to a
[`BulkSink`] as it arrives. Neither side buffers the whole transfer, so RAM is
**constant regardless of size** (only the slice wrappers are bounded, by their
slice), not capped by a whole-transfer buffer (~6 KB on this 20 KB part).
Verified on hardware to
**64 KB (1024 chunks)** with constant RAM (see `net_bulk_stream`).

```rust
// Streaming usage: implement the two traits over any backing store.
net.bulk_serve_from(NODE_ID, &mut source).await;   // BulkSource: read store → chunk
net.bulk_fetch_into(GW_ID, &mut sink).await;       // BulkSink: verify/hash/store each chunk
```

**OTA pairing (v3: X25519 key agreement, numeric comparison).** A node and a host with an open
pairing window derive the per-node link key together; no key travels on air. The exchange is
Bluetooth LE Secure Connections numeric comparison (Core Specification Vol 3 Part H 2.3.5.6.2)
with the node as the initiator and the host as the responder, HMAC-SHA256 / SHA-256 in place of
its AES-CMAC functions:

| # | Frame | Sender | Payload (bytes) | Sealed under | The sender computed first |
|---|---|---|---|---|---|
| 1 | JOIN_REQ | node, `dest = 0` | `3 ‖ E_node` (33) | `JOIN_FRAME_KEY` (public) | its ephemeral (before the attempt) |
| 2 | JOIN_RESP | host, `dest = node` | `3 ‖ E_gw ‖ C_gw` (49) | `JOIN_FRAME_KEY` | the commitment (one HMAC) |
| 3 | JOIN_NONCE | node, `dest = host` | `3 ‖ N_node` (17) | `JOIN_FRAME_KEY` | nothing |
| 4 | JOIN_REVEAL | host, `dest = node` | `3 ‖ N_gw ‖ tag_gw` (25) | `JOIN_FRAME_KEY` | shared secret + key schedule |
| 5 | JOIN_CONFIRM | node, `dest = host` | `3 ‖ tag_node` (9) | the NEW link key | commitment check, shared secret + key schedule |

The leading `3` identifies the handshake (a node or host built from an older checkout, whose
three-frame v2 started with `2`, is refused on it with a logged reason). Every payload fits the
74-byte MTU.

- **Commitment** (BLE f4): `C_gw = HMAC-SHA256(key = N_gw, "tower pairing v3 commit" ‖ node_addr
  LE ‖ gw_addr LE ‖ E_node ‖ E_gw)[..16]`. The host commits to its nonce `N_gw` before it sees
  `N_node`; the node sends `N_node` only once it holds `C_gw`, and accepts a JOIN_REVEAL only if
  its `N_gw` opens `C_gw` (otherwise `net: pairing: reveal from 0x… does not open its
  commitment, ignored`: never a key derived from it). Nonces are 16 bytes, fresh per handshake.
- **Key schedule** (`tower_net::handshake`, host-tested against RFC 7748 / RFC 5869 vectors and
  an independent reference; `crypto_x25519_kat` checks the same golden on the board): the
  transcript hash `th = SHA-256("tower pairing v3" ‖ node_addr LE ‖ gw_addr LE ‖ E_node ‖ E_gw ‖
  N_node ‖ N_gw)`; `shared = X25519(e_own, E_peer)` (an all-zero result, from a low-order point,
  is refused); `okm = HKDF-SHA256(salt = th, ikm = shared, info = "tower pairing v3")`, 32 B:
  link key `okm[0..16]`, confirm key `okm[16..32]`. The confirmation tags are
  `HMAC-SHA256(confirm_key, label ‖ th)[..8]` with labels `"gw"` / `"node"`. Both addresses, both
  public keys and both nonces are bound into the key, the tags and the SAS: a substituted key,
  nonce or address gives different keys and a failing tag.
- **Who proves what.** The node checks the commitment, then `tag_gw` before it confirms (the
  host holds the key); the host checks `tag_node` inside a frame that only opens under the
  derived key, and commits only then. A lost confirm leaves the host's window open (nothing
  installed).
- **The node's commit point: the first acknowledged uplink.** Nothing acknowledges the
  JOIN_CONFIRM, so `join` returns the key **provisionally**: the host may never have committed it
  (a lost or late confirm, a full or failing registry). A node that persisted it there would
  believe it is paired while the host drops all of its traffic as an unregistered sender. So a
  product node installs the key in RAM only and sends a confirmed uplink under it; an ACK, which
  only a holder of the key can seal, proves the host installed it, and only then is the key
  persisted (`tower_net::pairing::commit_step`, re-exported as `tower::radio::net::commit_step`).
  The round: `COMMIT_SETTLE_MS` (500 ms) after the confirm, then up to `COMMIT_ATTEMPTS` (6)
  confirmed sends 2 s apart, outlasting the host's worst post-confirm stall
  (`GATEWAY_COMMIT_STALL_MS`, a registry write absorbing a synchronous EEPROM compaction; pinned by
  a host test). No acknowledgement: the key is discarded and the node is unpaired again, free to
  retry. The product gateway closes its window on the commit and serves the node's uplinks from
  its next loop turn (docs/gateway.md). The residual asymmetry (the host committed but every
  acknowledgement was lost) leaves a registered node that can re-pair only after it is removed
  on the host, like any node its host still knows.
- **SAS** (BLE g2): `u64 LE(SHA-256("tower pairing v3 sas" ‖ th)[..8]) mod 10⁶`. Both ends log
  `net: pairing: SAS 123456`, and both hand it to their caller (`PairedPeer::sas`): the product
  gateway returns it in its `Paired` answer, so `tower nodes add --ota` prints it next to the
  node's address, and the node repeats it in its `node: paired with gateway` line. Its real guarantee: an
  active man in the middle that runs one
  handshake with each side shows the same 6 digits on both with probability **10⁻⁶ per attempt,
  whatever its computing power**, IF the operator compares the two values. The commitment is
  what makes that true: whoever plays the host towards the node is bound to a nonce chosen
  without knowing `N_node`, and whoever plays the node towards the host must send its nonce
  without knowing `N_gw`, so one side's SAS is always a random value the attacker could not
  steer (the three-frame v2 let it pick its key after seeing the node's and search ~10⁶ X25519
  operations for a collision). The attacker can still abandon an attempt whose two values differ
  (the node retries, silently), so a window gives it one 10⁻⁶ chance per attempt it can provoke:
  about 15 in a one-minute window (~4 s per handshake), ~2·10⁻⁵ in all. If nobody compares the
  SAS, an active attacker in range during the window is not detected. A passive eavesdropper learns nothing either way (the SAS is a function
  of the public transcript; the key needs a private key).
- **Ephemerals and nonces: TRNG only, one per request, zeroized.** Each side derives its X25519
  secret and its nonce from 96 bytes of the STM32 hardware TRNG (`board::trng_fill`; 64 bytes
  hashed with SHA-256 into the secret, 32 into the nonce). If the TRNG can't deliver, pairing is
  **refused** (`net: pairing: refused, TRNG unavailable (keys never come from the fallback)`),
  never keyed from the non-cryptographic fallback. The node draws a fresh pair for every
  JOIN_REQ, the host for every JOIN_REQ it answers (whether or not that handshake completes).
  Secrets, nonces until they are sent, the shared secret and the HKDF output are zeroized
  after use.
- **Timing.** One X25519 scalar multiplication takes **1175 ms** on the Core Module (16 MHz M0+,
  no 32x32→64 multiply; measured by `crypto_x25519_kat`), the handshake's critical path (shared
  secret + key schedule) 1185 ms. Each side computes its own public key ahead (the host when the
  window opens and again after each answered request, the node before transmitting), so exactly
  one scalar multiplication per side sits on the exchange, and the commitment round adds frames
  but none: the host computes after the JOIN_NONCE, the node after the JOIN_REVEAL. The waits:
  the node waits 300 ms for the JOIN_RESP and 300 ms + `PEER_KEX_TIME` (1.5 s, ~27 % over the
  measurement) for the JOIN_REVEAL; the host waits 300 ms for the JOIN_NONCE and 300 ms +
  `PEER_KEX_TIME` for the JOIN_CONFIRM. With no host answering, the node sends a request every
  `JOIN_ATTEMPT_PERIOD` (1.5 s: the request, the 300 ms wait, then idle), so a one-minute window
  (`PAIRING_WINDOW`) gives it ~40 attempts; a whole handshake takes ~3.8 s. One ephemeral serves
  the requests until a host answers (only its public key has been on air; its nonce leaves the
  node only in the JOIN_NONCE), then that handshake alone: the ~1.2 s of curve arithmetic per
  ephemeral holds the executor, and drawing one per attempt kept it busy ~80 % of the window, so
  the button missed presses. Too short a wait fails every pairing, too long only
  slows a retry: re-tune `PEER_KEX_TIME` from the KAT's `timing:` line if it moves.
- **The host's window has a fixed end.** `PairingHost::prepare(window)` fixes it before it draws
  the first ephemeral (~1.2 s), so the window closes `window` after the request that opened it.
  `open_pairing` takes a listen slice, never listens past the window, and answers a JOIN_REQ only
  while the whole host handshake still fits (`HOST_HANDSHAKE_TIME`: two turnarounds, two frames,
  both waits and its own shared secret, ~3.9 s; `tower_net::pairing::handshake_fits`), refusing it
  otherwise with `pairing: join request from 0x… refused: the window closes before a handshake
  could finish`. A handshake therefore never commits after the window closed, when the operator's
  tooling may already have reported a timeout. One `open_pairing` call lasts at most its slice
  plus one handshake plus one ephemeral (~7 s for the gateway's 2 s slices).
- **Framing.** `JOIN_FRAME_KEY` is public and never used for anything else: it only gives the
  four frames before the link key a CCM format, so a corrupted frame fails its tag and is
  dropped (its bytes still spell `v2`, so an older node's JOIN_REQ opens and is refused on its
  handshake byte). Duty mode only: under LBT+AFA or FHSS both sides refuse (a fixed-channel
  exchange).
- **Cost.** Roughly 16 KB of flash per image (X25519 + SHA-256 + HKDF + HMAC + the exchange: the
  ~22 KB of the three-frame v2, +2.5 KB for the commitment round, -9 KB since SHA-256 is built
  with sha2's compact software backend, the `sha2_backend_soft = "compact"` cfg in
  `firmware/.cargo/config.toml`, whose rounds run as a loop instead of unrolled; pairing does
  not feel the speed difference next to the 1.2 s scalar multiplication). Stack: the shared-secret
  computation needs ~2.5 KB below its caller (static estimate; `crypto_x25519_kat` reports the
  measured high-water), and it runs in non-inlined functions so that frame exists only while
  it computes.

**OTA pairing only adds new peers.** The host sends no JOIN_RESP for a JOIN_REQ whose address
is already a registered peer, reserved, or the host's own (a warning, at most one per 10 s with
the count of attempts it stands for: `pairing: join request from 0x… refused: … (n since the last
report)`). The key agreement is unauthenticated until someone
compares the SAS, so anyone could otherwise JOIN_REQ *as a victim*, complete a handshake and
re-key the victim out of the network. To re-pair a known node, remove it first (`remove_peer`;
the gateway's `NodeRemove`). The joiner accepts only host frames addressed to itself (`dest ==
addr`), and after the first JOIN_RESP only that host's (a reveal that opens the commitment and
carries a valid tag); the host accepts the nonce and the confirm only from that joiner,
addressed to it.

```rust
// host: fix the window, precompute its ephemeral and nonce (TRNG; None = refused), listen in slices
if let Some(mut host) = PairingHost::prepare(PAIRING_WINDOW) {
    if let Some(peer) = net.open_pairing(&mut host, PAIRING_WINDOW).await {
        let _ = net.add_peer(peer.addr, &peer.key); // the derived link key; show peer.sas
    }
}
// joiner: brings its own address; the result is provisional until an uplink under it is ACKed
if let Some(peer) = net.join(addr, PAIRING_WINDOW).await {
    let _ = net.add_peer(peer.addr, &peer.key); // RAM only
    // confirmed sends to peer.addr until commit_step(attempt, delivered) says Commit (persist
    // (peer.addr, peer.key)) or Discard (remove_peer(peer.addr))
}
```

**Keyed join (E4).** A node whose per-node key the host already holds joins under that key
instead, with no window and no key on air. The host registered it with `add_peer` (the
gateway's `NodeAdd`). The two cases it is for: a node whose key was provisioned by cable or in
the factory, which finds its gateway on air without a pairing window; and a replacement
gateway that imported the node keys (`NodeAdd` of each registry entry, with the counter floors its
`NodeRevealKey` export carried), which the nodes re-join without re-pairing:

```
node  JOIN_REQ  flags KEYED, src = node, dest = 0,    payload node(4 LE)          under the node key
host  JOIN_RESP flags KEYED, src = host, dest = node, payload req counter(4 LE)   under the node key, ACK_TURNAROUND later
```

- The host answers inside `recv()`, at any time, and only a **registered** peer whose
  request counter is **fresh** on its replay lane (`keyed_req_admit`); the lane advances, so
  a recorded request never draws a second response. The host proves the key and its
  freshness by echoing the request's counter (`keyed_resp_matches`): the joiner's TX
  counter never repeats, even across a reboot, so no random nonce is needed.
- Both ends prove the key: this is mutual authentication, and nothing secret travels.
- Nothing is committed on the host (the peer, key and lane are already installed);
  `take_keyed_join()` reports the joiner for the log. The joiner learns the host's address
  and installs it with `add_peer(gw_addr, key)`.
- Two frames, not three: the host's freshness proof is the echo and the joiner's is the
  lane, so a confirm would add nothing.
- Counters: both frames take their TX counter from the one allocator and are sealed before
  the first await (duty gate first, so a refused attempt burns no counter), so cancelling
  `recv` or `join_keyed` mid-flight can never leave a counter for the next frame to reuse.
  Both frames are metered by the duty governor like any other TX.
- **Duty mode only.** The exchange is a fixed-channel request/response, with no
  listen-before-talk and no hop schedule: under LBT+AFA or FHSS `join_keyed` returns
  `JoinError::WrongMode` without transmitting, and the host leaves the request unanswered.
- **Re-keying composes with the lane rule.** A node re-provisioned under a **new** key (cable
  `NodeAdd`) gets a fresh replay lane on the host (the lane record's key-check value no
  longer matches), so it can keyed-join even with a restarted counter. A counter that restarted
  under the **same** key is not a refused join but nonce reuse, already on air by the time the
  host refuses it (`stale counter` is detection, not protection), so it is prevented where
  counters restart: `Net::new` fails closed when the app holds stored keys but the watermark is
  gone, cable provisioning always mints a fresh key, and a key moved to another gateway carries
  its counter floors (*A key's counter history moves with the key*, above). A replacement gateway
  that imports the registry this way has its counter above the old one's, and the nodes accept it.
- **Hosting is opt-in; operator switch.** A device answers keyed joins only after
  `net.set_keyed_join(true)`; until then `recv` ignores keyed JOIN_REQs silently, and an image
  that never calls it does not link the responder at all (it is a function pointer installed
  by that call; ~1.2 KB of flash a node image doesn't need). `set_keyed_join(false)` keeps the
  responder but refuses every keyed join before any CCM work: the gateway calls it from its
  `keyed-join` setting (on by default, since the exchange is mutually authenticated). Every
  refusal is logged on the `net` target, rate-limited to one line per 10 s with a count:
  `keyed join from 0x0000ab12 refused: <reason> (<n> since the last report)`, the reason being
  one of `keyed joins are switched off`, `radio not in Duty mode`, `not a registered node`,
  `does not open under its registered key`, `malformed request`, `stale counter ...` (with the
  counter and the last-seen value), `TX locked (nonce safety)`, `duty-cycle limited`. A
  response that could not be sent is logged, on the same limit, as `keyed join from 0x0000ab12:
  response not sent (<n> since the last report)`.
- The request goes to `dest = 0`, so a device that is not a key holder drops it: an
  unregistered `src` is refused before CCM, and a frame under another key fails CCM.
- In a P2P mesh any peer holding the joiner's key answers; the joiner then talks to that
  peer. In a star the joiner's only key holder is its gateway.

```rust
// host: answer keyed joins; the key was installed up front (NodeAdd); recv() answers inline
net.set_keyed_join(true);
let _ = net.add_peer(node_addr, &node_key);
if let Some(node) = net.take_keyed_join() { /* log */ }
// joiner: same key, own address; returns the host's address
match net.join_keyed(addr, &node_key, Duration::from_secs(10)).await {
    Ok(gw_addr) => { let _ = net.add_peer(gw_addr, &node_key); }
    Err(e) => { /* JoinError: Timeout, WrongMode (AFA/FHSS), NonceLocked, BadAddr */ }
}
```

**Duty governor.** A token-bucket meters **all** TX airtime (data, ACKs,
bulk, pairing); a send that would exceed the budget returns `DutyLimited` instead
of transmitting. The policy follows the band and is reselected by `set_band`: EU
868 enforces the 1 % / hour limit; US 915 is unrestricted (FCC 15.247 governs by
channel-dwell/PSD, not a fixed duty cycle). Time-on-air is computed per frame
length from the 19.2 kbps rate. Verified independently by `net_duty_kat`.

## Spectrum-access modes (polite high-power operation)

Beyond plain duty-cycled access, two region-specific modes give "polite" channel
access, selected at runtime (mutually exclusive, like `set_band`; `Net::access()`
reports the current [`Access`]). The arbitrary-frequency retune primitive
[`config::set_freq_hz`] (SYNT rewrite, CHNUM=0) underpins both.

**EU 868 LBT + AFA (EN 300 220): implemented, hardware-verified.** Listen-
Before-Talk + Adaptive Frequency Agility relaxes the 1 % duty cap. No time-sync:
the node listens (CCA) before every TX and hops to another of 8 channels
(865.2-868.0 MHz) when one is busy or in its post-TX off-time; the gateway scans
the set and ACKs on the catching channel. `net.enable_afa(role, cfg)` →
`afa_send` / `afa_serve`. Example `radio_afa` (verified: confirmed delivery + the
agility channel sweep + gateway-scan rendezvous on two boards).

> **Verify before any product claim:** the EN 300 220 CCA time/threshold,
> minimum channel count, and off-time here are bench defaults; confirm against the
> current standard (and that the chosen sub-band permits LBT+AFA in lieu of duty).

**US 915 FHSS (FCC §15.247): implemented, hardware-verified.** 80-channel
frequency hopping (903.0-926.7 MHz, 300 ms slots, 24 s cycle), gateway = hop
time-master + per-slot beacon, node blind-rendezvous on a fixed channel then hops
in lockstep, re-aligning on each beacon. `net.enable_fhss(role, cfg)` →
`fhss_master_tick` (gateway loop) / `fhss_node_tick` + `fhss_send` (node loop).
Compliance is **structural**: N=80, cycle 24 s > 20 s ⇒ each channel is tuned at
most once per 20 s ⇒ ≤ one 300 ms slot occupancy (25 % under the 0.4 s/20 s limit),
no per-channel governor needed; a light `[u16; 80]` airtime counter feeds the
compliance histogram. Examples `radio_fhss` (verified: node `locked to the hop schedule`, then
confirmed delivery on hopping channels; `sync lost, rescanning` and a relock on gateway loss) and
`fhss_compliance` (≥50 channels used, max per-channel airtime ≪ 0.4 s, band edges
exercised). The hop seed is key-derived (not sent on air).

> **Verify before any product claim:** the exact §15.247 channel/dwell numbers
> (FCC KDB) and that the implementation meets them under your antenna/power.

## Examples

Two-board examples are one source file built twice with a role feature (e.g.
`TOWER_FEATURES=role-node just flash example net_confirmed`).

| Example | Boards / roles | What it shows |
|---|---|---|
| **`radio_gateway`** / **`radio_node`** | gateway / node | **reference apps**: confirmed, secure telemetry uplink + decode |
| `radio_id` | 1 | device-ID check (SPI/CS/SDN bring-up) |
| `radio_state` | 1 | READY/STANDBY transitions + nIRQ |
| `radio_cw` | 2 | CW carrier (TX) detected via partner RSSI (RX) |
| `radio_beacon` | 2 | raw TX/RX link, per-packet RSSI/LQI/SQI/AFC |
| `radio_regdump` | 1 | read back the RF registers after config |
| `radio_linkdiag` | 2 | RX event/quality diagnostics over a live link |
| `radio_csma` | 2 | CSMA/CCA defers TX while a jammer holds the channel |
| `radio_sleep` | node / gateway | SLEEP vs SHUTDOWN wake latency + re-link |
| `crypto_aes_kat` | 1 | AES-128 ECB FIPS-197 known-answer test |
| `crypto_ccm_kat` | 1 | AES-128-CCM RFC 3610 vector + tamper test |
| `crypto_frame_loopback` | 1 | frame codec + nonce + CCM round-trip |
| `crypto_x25519_kat` | 1 | pairing-v3 crypto: X25519 vs RFC 7748, the handshake golden (commitment, key, SAS, tags), timing, stack high-water |
| `net_secure_ping` | node / gateway | one CCM-sealed DATA frame end-to-end |
| `net_confirmed` | node / gateway | confirmed delivery + ACK + retransmit |
| `net_persist` | 1 | TX-counter reserve-ahead survives reboot |
| `net_duty_kat` | 1 | duty-governor token-bucket KAT |
| `net_bulk` | gateway / node | bulk pull: announce → BULK_REQ/BULK_DATA |
| `net_bulk_stress` | gateway / node | large bulk (multi-KB) + CRC-32 + throughput stress |
| `net_bulk_stream` | gateway / node | streaming bulk (source/sink), 4-64 KB, constant RAM |
| `net_pairing` | gateway / node | OTA pairing: X25519 key agreement with a commitment round derives a per-node key, SAS on both ends |
| `net_keyed_join` | gateway / node | keyed join (E4) under a per-node key both ends hold |
| `net_star` | gateway / node[,node-2] | star: per-node keys + per-node replay lanes |
| `net_p2p` | role-peer-a / role-peer-b | P2P bidirectional confirmed exchange |
| `net_channel` | node / gateway | secured link on a non-default channel (VCO recal) |
| `radio_band` | node / gateway | runtime 868↔915 switching via `set_band` (live retune) |
| `radio_afa` | node / gateway | EU LBT+AFA: listen-before-talk + frequency agility (EN 300 220) |
| `fhss_sweep` | 1 | FHSS channel-plan + 80-channel synth lock + GUARD measure (F1) |
| `fhss_kat` | 1 | FHSS hop-permutation / dwell / beacon-frame KATs (F3-F5) |
| `radio_fhss` | node / gateway | US FHSS link (FCC §15.247): lock + hopping confirmed delivery |
| `fhss_compliance` | 1 | §15.247 evidence: channel count + per-channel airtime histogram |
| `edge_frame_limits` | 1 | MTU + malformed/forged-frame rejection KAT |
| `edge_recovery` | 1 | RX-timeout / stuck-state / FIFO recovery |
| `edge_rapid` | node / gateway | back-to-back confirmed, strict-monotonic counters |
| `radio_interop` | node / gateway | semi-fuzzy soak: randomized traffic + invariant checks |

## A note on RX completion (hard-won)

`RX_DATA_READY` only fires if the **RX-timeout stop condition** is configured.
At reset, `PCKT_FLT_OPTIONS.RX_TIMEOUT_AND_OR_SELECT` = 1 means "the timeout
cannot be stopped" (datasheet Table 30 / §9.3): a complete packet lands in the RX
FIFO but the part sits in RX forever and never raises the interrupt. Clearing that
bit (`PCKT_FLT_OPTIONS` bit6 = 0, with no timeout masks) selects "reception ends
at the reception of the packet", so `RX_DATA_READY` fires normally. `config::apply`
sets this; it is unrelated to the RF/demod registers.

**Frames straddling an RX deadline.** `rx()` also unmasks `VALID_SYNC`. A sync IRQ means a
frame is already on the air: `rx()` does *not* re-arm (that flush would kill it) and, if the
frame would outlive the caller's deadline, extends that deadline **once** by `SYNC_HOLD`
(max-frame ToA + 10 ms ≈ 55 ms). Previously the deadline branch SABORTed a half-received frame;
at the gateway's 250 ms receive slices that lost ~7-11 % of first-attempt uplinks. An idle
RX timeout (no sync in flight, ≥ 20 ms in RX) caches the RSSI latched by its SABORT, readable
via `take_idle_rssi()`, the gateway's ambient-channel source, replacing `rssi_sample()`'s
separate ~20 ms deaf window.

## Parameters reference

| Constant | Value |
|---|---|
| Device ID / crystal | part 304 / version 48 · **50 MHz** (not a TCXO) |
| Bands / channels | EU 868 & US 915 · 3 per band |
| EU 868 channels | 868.1 / 868.3 / 868.5 MHz (200 kHz spacing; "g1" ≤ +14 dBm ERP, ≤ 1 % duty) |
| US 915 channels | 915.0 / 915.2 / 915.4 MHz (200 kHz spacing). Single-channel is **bench-only**; use FHSS for a compliant US link |
| Modulation / bit rate / deviation / BT | GFSK / 19 200 bps / 20 kHz / 1 |
| RX bandwidth | ~216 kHz (wide; narrow via AFC_CORR, see Design rationale) |
| Sync / CRC / preamble | `0xDB624715` / 16-bit `0x1021` / 4 B |
| FIFO = network frame | 96 B |
| Header (non-bulk / bulk) / CCM tag | 14 B / 17 B / 8 B |
| Payload (single / bulk chunk) | ≤ 74 B / 64 B |
| Nonce | 13 B (`src ‖ counter ‖ bulk_index ‖ 0x0000`) |
| Time-on-air (max frame / ACK) | ≈ 44.6 ms (96 B) / ≈ 16.25 ms (28 B: 14 header + 6 payload + 8 tag; 17 ms in the duty budget, `frame_toa_ms` rounds up) |
| TX power (default / max) | +11.6 dBm |
| CSMA RSSI threshold / max backoff | −90 dBm / ~100 ms |
| ACK window / inter-rep backoff / RX→TX turnaround | 200 ms / random 0-100 ms / ~20 ms |
| Confirmed repetitions (range / default) | 1-10 (`MAX_REPS`) / 3 |
| Re-ACKs per received counter | ≤ 9 (`MAX_REPS − 1`; reset by the next fresh frame) |
| RX sync-hold (deadline extension, once per `rx`) | ≈ 55 ms (`SYNC_HOLD`) |
| Replay-lane record (`NS_NET` 0x10-0x1F) | 12 B `addr ‖ last_seen ‖ KCV` (any other length is invalid) |
| Confirmed latency (N=3 / N=10) | ≈ 1.06 s / ≈ 3.55 s worst case |
| Replay counter | 32-bit; starts at 1; **saturating** at 2³²−1 (fail-closed, not wrap); re-key well before |
| Reserve block `RESERVE` / lazy-persist `P` | 1024 / 32 transfers (`P` = 1 on a sleeping node, `set_lane_persist_period`) |
| Max bulk / chunk index | 1 GiB (`MAX_BULK_LEN`: 2²⁴ chunks × 64 B, all a 24-bit index addresses; both ends refuse a longer transfer) / 24-bit |
| Bulk idle timeout | 30 s |
| Star nodes / P2P peers | ≤ 16 / ≤ 8 |
| OTA pairing: window / peer computation budget / host handshake | 60 s (`PAIRING_WINDOW`) / 1.5 s (`PEER_KEX_TIME`) / ≈ 3.9 s (`HOST_HANDSHAKE_TIME`, the least window left to answer a JOIN_REQ) |
| OTA pairing: joiner commit round | 500 ms settle, then ≤ 6 confirmed sends 2 s apart (`COMMIT_SETTLE_MS` / `COMMIT_ATTEMPTS` / `COMMIT_RETRY_MS`) |
| Reserved IDs | `0x00000000`, `0xFFFFFFFF` (never a node/gateway/peer address; a peer at our own address is refused too) |
| Frame format marker | 1 (`bits[7:5]` of the first byte; fixed, not a version) |
| FHSS (US) | 80 channels · 300 ms slot · 24 s cycle |
| EU duty cycle | ≤ 1 % per sub-band per device (governed; all TX counted) |

## Design rationale

The reasoning behind the chosen RF and security parameters: what to understand
before changing any of them.

**RX bandwidth (the central RF trade-off).** ±40 ppm crystal error per unit means
two units differ by **~80 ppm** worst case. At 868 MHz that's a ~69 kHz carrier
offset; with the ~59 kHz GFSK signal (Carson) the RX filter must pass *signal +
2·offset* ≈ **198 kHz** (≈ 206 kHz at 915), so the nearest SPIRIT1 step **≥ ~210 kHz**
is required for bring-up. **AFC re-centers the residual, but the analog filter must
still *pass* the offset signal**, so a wide filter is unavoidable until the real
drift is measured. ±40 ppm is conservative (the SPSGRF is a plain 50 MHz crystal, not
a TCXO); the SPIRIT1 reports each packet's offset in `AFC_CORR`, so the path to
narrowing is: run wide, log `AFC_CORR` between real boards over temperature, then set
RX BW to the measured worst case + margin (a true ~±20 ppm part → ~130 kHz → ~2 dB
more range). Until narrowed below ~180 kHz the 200 kHz channel spacing overlaps, so
only ch0/ch2 are simultaneously usable; all three after narrowing.

**Channels are tuned, not spanned.** The receiver sits on **one** channel; the RX BW
is crystal-drift tolerance *around that center*, not a window over all three; a
receiver on one channel does **not** hear the others, so **a node and its gateway must
share a channel**. The 3 channels exist to run co-located networks without
interference, not for a node to roam. **Band** is region/config-time (EU→868, US→915),
identical on a gateway and all its nodes, with no OTA negotiation. EU 868 uses the
868.0-868.6 MHz "g1" sub-band (EN 300 220 / ERC 70-03): **≤ +14 dBm ERP, ≤ 1 % duty**
(keep conducted +11.6 dBm to ≤ ~2 dBi antenna, else reduce power). Duty is **per
sub-band, per device** and counts **all** TX (data, ACK, bulk, retransmit, JOIN), and
the gateway is governed too. **Final regulatory compliance (the current EN 300 220
revision, an FCC strategy for the US, ERP with the real antenna, lab testing) is the
integrator's responsibility before shipping.**

**Security model (AES-128-CCM): the nonce-uniqueness argument.** Each node has its
own AES-128 key (the gateway holds one per node; the **same key protects both
directions**). The cleartext header is the **AAD**, the payload is encrypted, and the
8-byte tag authenticates both. The 13-byte nonce is *derived*, never transmitted:
`src ‖ counter ‖ bulk_index ‖ 0x0000`. It is unique per `(key, frame)` because: the
key is per-node and **`src` fixes the sender** (the two directions never collide even
at equal counter values; no `dir` field needed); the 32-bit **counter** advances one
per transfer; **`bulk_index`** separates the chunks of one transfer; and a
**retransmission re-sends the byte-identical frame** (same counter ⇒ same ciphertext ⇒
safe). An **ACK therefore uses the ACKer's own fresh counter** (the *acknowledged*
counter rides in the ACK payload, never as the nonce counter), so an ACK and the frame
it answers never share a nonce. On receive, **CCM-verify first** (this authenticates
the header, including the counter), **then** compare to the per-peer last-seen and
update it, so a forged high counter can't poison replay state, and tampered frames are
dropped before the network layer acts. On key install the TX counter starts at **1**
(`0` = never sent) and last-seen at **0**; a **re-key resets both** (a new key is a
disjoint nonce space, so old ciphertext can't replay under it). The counter **saturates**
at 2³²−1 instead of wrapping: at the ceiling it sticks, and the strict `counter > last-seen`
rule makes the peer reject every further frame as a replay, so the link fails **closed**
rather than silently reusing a low nonce. Re-key well before then (≈136 yr at 1 Hz, so not
a practical limit). EEPROM key storage is only as safe as the chip's readout protection;
**enable flash RDP for production**.

## Known limitations & caveats

- **OTA pairing is unauthenticated unless the SAS is compared.** A passive eavesdropper learns
  nothing (X25519), but an active attacker in range during the window can sit between the two
  ends; comparing the `pairing: SAS` lines detects that except with probability 10⁻⁶ (the
  commitment round leaves the attacker nothing to search; see *OTA pairing*). Nothing detects it
  if nobody compares. Keep windows short and user-initiated; enable flash RDP for production key
  storage. Where the key can be provisioned out of band, the keyed join (E4) needs no window at
  all.
- **US 915 single-channel `Us915` is bench-test only** (runtime-switchable via
  `set_band`, hardware-verified, but **not** FCC 15.247-compliant; use FHSS for a
  compliant US link). EU 868 (duty or LBT+AFA) is the compliant default region.
- **FHSS sync** is robust. The node opens its beacon RX a guard *before* each slot
  boundary (armed before the gateway transmits), listens a wide window that ignores
  stray frames, and (crucially) **rides through a fade by predicting the channel
  on its kept clock anchor** (drift ≪ the RX window for dozens of slots), re-locking
  within one slot once RF returns rather than treating a missed beacon as a loss.
  Only after ~7 s of misses (anchor too stale / gateway restarted) does it fall back
  to rendezvous scanning. **Soak-verified: 1 lock, 0 sync losses, 1133 confirmed
  deliveries over ~6 min / ~15 cycles, max delivery gap 2 slots** (an earlier
  per-slot-strict design dropped sync 1-2×/6 min with ~23 s re-acquire each). A
  *gateway restart* still forces a one-cycle re-acquire (new epoch); that is inherent.
  (`RAM note:` the FHSS per-channel state is a `[u16; 80]` counter; an earlier
  `[DutyGovernor; 80]` overflowed the 20 KB L0.)
- **RX bandwidth is set wide (~216 kHz)** to tolerate the 50 MHz-crystal tolerance
  without lab instruments; narrowing it (per the AFC-vs-temperature data; see Design
  rationale) is a future optimization. All three EU channels are usable as-is.
- **Counter persistence uses a single reserve-ahead watermark cell** (RESERVE=1024;
  ~10⁸ transfers before EEPROM wear matters). A multi-cell wear-ring is a refinement.
- **A *stale older* watermark is not detectable.** A lost watermark record fails closed (see
  *Counters, replay & persistence*), but if mid-log EEPROM corruption orphans the newest
  watermark while an *older* watermark record of the same key still precedes the bad record in
  the same KV half, the store reads back that older value and the boot resumes below counters
  already sent. Nothing else in `NS_NET` records our own TX counter, so the net layer can't tell;
  closing it needs the KV layer to report mid-log corruption (valid records *after* the first bad
  one, unlike a benign torn tail) so `Net::new` can fail closed. A torn write at the log tail
  (the normal power-loss case) is safe.
- **Lane-local eviction.** Lane records are addr-keyed in 16 locals; only after more distinct
  peers than that have ever been registered does a new peer evict a *removed* peer's record. That
  evicted peer, if later re-added under the **same** key, restarts its replay window at 0;
  re-add it under a fresh key.
- **Half-duplex single radio.** `Net` serializes one transfer at a time; CSMA
  mitigates contention but cannot eliminate hidden-node collisions; confirmed
  delivery + retransmit absorbs the rest.
- **`Net::send` does not enable CSMA by default** (CSMA is a radio-layer feature
  shown in `radio_csma`); wire `use_csma=true` into the TX path if your deployment
  needs it on every frame.
