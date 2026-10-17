# TOWER security model

This document describes what the TOWER stack protects, against whom, how, and what it does not
protect. It is written for an external security reviewer and for anyone changing
security-relevant code. Reporting a vulnerability is covered by [SECURITY.md](../SECURITY.md).

Status: 2026-10-17. Single-board behaviour is hardware-verified on a Core Module; the two-board
radio paths (pairing, keyed join, end-to-end gateway traffic) are verified by host tests and
reasoning, and still need an on-air run before a production claim.

## 1. System and trust boundaries

```
  push-button / sensor node                Radio Dongle (gateway firmware)          gateway host
  ┌──────────────────────┐   sub-GHz RF   ┌──────────────────────┐   USB serial  ┌────────────────────────┐
  │ firmware/ (tower SDK) │ ◀────────────▶ │ firmware/ (gateway)   │ ◀───────────▶ │ cli/ `tower gateway`    │
  │ STM32L083 + SPIRIT1   │  AES-128-CCM   │ node registry, queue  │  framed       │ MQTT bridge + broker    │
  └──────────────────────┘                └──────────────────────┘  console      └────────────────────────┘
          ▲  USB console (physical access)                                              ▲  MQTT (loopback or LAN)
          │                                                                              │
     an operator with a cable                                                   local processes, LAN clients
```

| Boundary | Protected by | Trust assumption |
|---|---|---|
| Radio (node to gateway) | AES-128-CCM per node key, replay lanes, counters | the air is hostile |
| USB console (any board) | nothing cryptographic | whoever holds the cable is trusted |
| Gateway host to MQTT clients | broker policy (loopback anonymous, else login) | loopback = every local process is trusted |
| Physical board (SWD, ROM bootloader) | optional readout protection (RDP level 1) | without RDP, holding the board = holding its keys |
| Build and release pipeline | pinned toolchain and actions, lockfiles, attestations | GitHub and crates.io are trusted infrastructure |

## 2. Assets

- **Per-node link keys** (AES-128): one per node, stored in the node's and the gateway's data
  EEPROM. Whoever has a node key can read and forge that node's traffic.
- **TX counters**: the CCM nonce is `src ‖ counter ‖ bulk_index ‖ 0x0000`, so a counter that ever
  repeats under the same key is a full CCM break (keystream and tag forgery). Counter integrity is
  as important as key secrecy.
- **The gateway's node registry** (who is allowed to talk) and **downlink queue** (remote shell
  lines queued for sleeping nodes).
- **Control of the gateway** through MQTT: add, remove or re-key nodes, open pairing windows,
  read keys (`reveal_key`), queue remote shell lines.
- **Firmware integrity**: what runs on the boards.

## 3. Attackers considered

1. **Passive RF listener** in range, recording everything, including pairing exchanges.
2. **Active RF attacker**: injects, replays, delays and jams frames; can be present during a
   pairing window and run a man-in-the-middle.
3. **Network attacker** on the gateway host's LAN (reaches the MQTT port if it is exposed).
4. **Local unprivileged process or user** on the gateway host.
5. **Physical attacker** holding a node or gateway (USB, SWD, the ROM bootloader).
6. **Supply-chain attacker**: a compromised dependency, GitHub Action or release artifact.

Out of scope: RF jamming as a denial of service (inherent to radio), side channels on the
STM32L0 AES engine, attacks requiring decapping the chip.

## 4. Mechanisms

### 4.1 Radio frames (`firmware/src/radio`, `crates/net`, `crates/radio`)

- **AES-128-CCM** on the STM32L0 hardware AES engine (N = 13, L = 2, 8-byte tag). The whole
  cleartext header (`format/type | flags | src | dest | counter | [bulk_idx]`) is the AAD, the
  payload is encrypted. Verified against FIPS-197 and RFC 3610 vectors on hardware
  (`crypto_aes_kat`, `crypto_ccm_kat`, `crypto_frame_loopback`).
- **Nonce uniqueness**: one allocator (`TxCounter::alloc`, used by every TX path: data, ACK,
  beacon, bulk, pairing, keyed join) hands out a counter that is already consumed before the
  caller seals or awaits anything, so a cancelled transfer can never leave a counter for reuse.
  The counter persists through a reserve-ahead watermark (RESERVE = 1024): boot resumes at the
  last durable watermark. Any network record without a readable watermark (a lost watermark),
  a failed or unverifiable watermark write, and a watermark at the `u32` ceiling all start TX
  **locked** (fail closed: receive still works, transmit refuses).
- **Replay protection**: a per-peer replay lane (highest accepted counter) persisted in EEPROM,
  each lane record carrying a 4-byte key-check value (first 4 bytes of AES_K(0^128); CCM never
  encrypts an all-zero block, so it leaks nothing usable). A lane is restored only under the same
  key, so a re-keyed node starts a fresh window instead of being dropped as a replay. Counter 0 is
  always a replay. Re-ACKs of a retransmitted frame are capped per counter.
- **Registered peers only**: the gateway and the push-button drop any sender that is not in the
  peer table on its cleartext header, before CCM work, before any ACK and before any EEPROM write.
  (A shared default-lane key existed and was computable from public data; it is disabled in the
  products.)
- **Gateway forwarding**: the host bridge publishes uplinks only for registered nodes.

### 4.2 Getting a key onto a node

Three paths; each refuses an address that is already registered, reserved (0, broadcast) or the
gateway's own.

1. **Cable provisioning** (`tower nodes add --port`): the host generates the key with the OS
   CSPRNG and writes it to the node and the gateway over USB. Nothing crosses the air.
2. **OTA pairing v3** (`tower nodes add --ota`, an operator-opened window): X25519 key agreement
   with a commitment round modelled on Bluetooth LE Secure Connections numeric comparison:

   | Frame | Content |
   |---|---|
   | JOIN_REQ (node) | node ephemeral public key |
   | JOIN_RESP (gateway) | gateway ephemeral public key, commitment to the gateway nonce |
   | JOIN_NONCE (node) | node nonce (16 B) |
   | JOIN_REVEAL (gateway) | gateway nonce, gateway confirm tag |
   | JOIN_CONFIRM (node, under the new link key) | node confirm tag |

   One SHA-256 hash of the whole transcript (both addresses, both public keys, both nonces) feeds
   HKDF for the link key, both confirm tags and a 6-digit short authentication string (SAS) that
   both ends log. Ephemeral secrets and nonces come from the hardware TRNG only (pairing is refused
   if the TRNG is unavailable) and are zeroized after use. The registry entry is committed only
   after a valid node confirm. Cost on the Core Module: one X25519 scalar multiplication per side
   during the exchange (measured 1175 ms at the firmware's clock; RFC 7748 vectors verified on
   hardware by `crypto_x25519_kat`).
3. **Keyed join** (E4): a node that already holds its per-node key (cable or factory
   provisioned, or a replacement gateway that imported the node keys) finds its gateway with two
   frames sealed under that key; both ends prove the key, nothing secret travels, and a recorded
   request is a replay. The responder is opt-in per device and switchable on the gateway
   (`keyed-join` setting). Duty-cycle mode only.

### 4.3 USB console (`crates/protocol`, `firmware/src/console.rs`, `cli/`)

- No cryptography: physical access to the cable is the trust boundary.
- Decoding is hardened against hostile bytes from either end: COBS framing with bounded buffers,
  CRC-32 checked before anything else, exact-length postcard decoding (trailing bytes rejected),
  fuzzed (`fuzz/protocol_frames`, `fuzz/protocol_radio`).
- Every frame carries a 16-bit schema tag folded from a structural hash of the message schema;
  a peer built from another source tree is rejected on its first frame (this is mismatch
  detection, not a security control).
- Destructive device commands (`/system/eeprom wipe`, address changes) are refused over the radio
  remote shell (`R_DENIED`); they run only from the USB console. `/system reboot` is allowed
  remotely.
- The host CLI escapes control characters from device and MQTT text on every plain output path
  (no terminal escape injection); the TUI is immune by construction.

### 4.4 Gateway host and MQTT (`cli/src/gateway`, `cli/src/client`)

- The embedded broker is **anonymous on a loopback bind** (the default, 127.0.0.1:1883) and
  **refuses to start on any other bind without a login** (`--mqtt-user` + `TOWER_MQTT_PASSWORD`);
  a configured login is enforced on any bind. No TLS (deferred; use a tunnel or VPN for remote
  access).
- Retained command messages are ignored and cleared; payload sizes and request ids are bounded;
  duplicate requests (QoS 1 redelivery) are dropped; node names are printable and unique; keys
  are redacted in the TUI's MQTT mirror.

### 4.5 Storage (`crates/kv`, `firmware/src/storage.rs`)

- Append-only key-value log in two EEPROM halves with a compaction flip; every record is
  CRC-protected; a torn write at the tail is ignored; the 2-byte tag after a new record is cleared
  before appending, so stale bytes can never be read back as records (a fuzz finding: without it,
  an old TX watermark could resurface after corruption, a nonce-reuse path).
- Factory reset (`/system/eeprom wipe confirm`) is a power-loss-safe flip to an empty store,
  freezes the store until reboot and keeps the wear counter.
- Fuzzed with power cuts at every write against an independent model (`fuzz/kv_store`).

### 4.6 Physical access (`crates/boot`, `tower protect`)

- Without readout protection, the ROM bootloader and SWD can read flash and the data EEPROM,
  i.e. every key on the board.
- `tower protect` sets **RDP level 1** (an explicit, confirmed, opt-in production step): flash and
  EEPROM can no longer be read over the bootloader or a debugger. `tower unprotect` regresses to
  level 0, which mass-erases flash and EEPROM (keys and pairing are gone; the node must be
  re-flashed and re-paired). RDP level 2 (permanent) is unreachable from the tooling: the
  bootloader client refuses writes outside main flash, so option bytes cannot be written.
- The MCU's 96-bit unique ID is shown for inventory only; it is public and authenticates nothing.

### 4.7 Supply chain and CI (`.github/`)

- One pinned Rust toolchain (1.99.1), committed lockfiles, `--locked` everywhere, `cargo audit`
  on every lockfile in CI, Dependabot (cargo, actions, toolchain) with a cooldown.
- Every GitHub Action pinned to a full commit SHA; workflows default to a read-only token;
  write permission only in the release publish job.
- Releases (`cli-v*` tags) build `tower` archives with `--locked`, publish SHA-256 checksums and
  signed build-provenance attestations (`gh attestation verify <archive> --repo hardwario/tower`),
  and gate on: tag equals the crate version, the commit is on `main`, CI passed for that commit.
- Repository rulesets: no force-push or deletion on `main`; all tags are immutable. Secret
  scanning with push protection, private vulnerability reporting, Dependabot security updates,
  immutable releases and CodeQL (Actions, Python) are enabled.

## 5. Residual risks and known limitations

1. **OTA pairing is only as strong as the SAS check.** A passive listener learns nothing. An
   active attacker present during the window can run a man-in-the-middle; comparing the two
   `pairing: SAS` lines detects it except with probability 10^-6 per attempt (the commitment
   round leaves nothing to search, but a failed attempt can be retried within the window). The
   node has no display: its SAS is only visible on its USB console. Prefer cable provisioning or
   keyed join where possible, and keep windows short.
2. **No forward secrecy for data.** Link keys are static per node; whoever obtains a key (e.g. a
   board without RDP) can decrypt recorded past traffic of that node and impersonate it until it
   is re-keyed.
3. **Keys at rest are readable without RDP.** Development boards are open by default.
4. **No firmware authenticity.** Anyone with bootloader access can flash arbitrary firmware; RDP
   level 1 protects readout, not code integrity. There is no secure boot or signed update.
5. **Loopback MQTT is anonymous**: every local process on the gateway host can control the
   network, read keys and queue remote shell lines. No TLS for remote brokers.
6. **A stale older watermark is not detectable**: if EEPROM corruption hides the newest watermark
   record while an older one still precedes it, boot resumes below counters already sent
   (counter reuse). A torn write at the log tail (the normal power-loss case) is safe; closing
   this needs the store to report mid-log corruption so the network layer can fail closed.
7. **EU duty-cycle budget starts full on each boot**, so a forced reboot loop can exceed 1 %/h
   (bounded by the boot-loop backoff); acknowledgements in AFA mode skip listen-before-talk (to be
   checked against EN 300 220's acknowledgement provisions).
8. **US 915 single-channel operation is bench-only** (not FCC 15.247 compliant); the product
   apps do not yet use FHSS (see firmware/TODO.md).
9. **Lane eviction**: after more distinct peers than the 16 lane slots have ever been registered,
   a removed peer's lane can be evicted; re-adding that peer under the same key restarts its
   replay window. Re-add under a fresh key.

## 6. Questions for the reviewer

1. The nonce uniqueness argument (section 4.1): one allocator, consumed before any await,
   reserve-ahead watermark, fail-closed boot conditions. Is there a path that reuses
   `(key, src, counter, bulk_index)`?
2. Pairing v3 (section 4.2): is the adaptation of BLE Secure Connections numeric comparison (SHA-256
   / HMAC-SHA256 / HKDF in place of AES-CMAC f4/f5/g2, our frame sequence) sound? Is the transcript
   binding complete? Does the refusal logic leave a window for registry takeover?
3. Keyed join: replay and reflection resistance across reboots of either side.
4. The key-check value in lane records (AES_K(0^128) truncated): any interaction with CCM that
   makes it a leak?
5. The storage log's power-loss and corruption behaviour (section 4.5) with respect to the
   counter watermark (residual risk 6).
6. The MQTT access policy for a gateway host shared with other services.

## 7. Where things live

| Topic | Code | Docs |
|---|---|---|
| CCM, frame format, nonce | `firmware/src/radio/{aes,ccm,frame}.rs` | `firmware/docs/radio.md` |
| Counters, replay, pairing, keyed join kernels | `crates/net` (host-tested) | `firmware/docs/radio.md` |
| Network layer runtime | `firmware/src/radio/net/` | `firmware/docs/radio.md` |
| Gateway registry and queue | `crates/gateway`, `firmware/apps/radio_dongle_gateway.rs` | `firmware/docs/gateway.md` |
| Console framing and schema tags | `crates/protocol` | `crates/protocol/README.md` |
| Storage | `crates/kv`, `firmware/src/storage.rs` | `firmware/docs/storage.md` |
| Readout protection | `crates/boot/src/rdp.rs`, `cli/` | `crates/boot/README.md`, `cli/README.md` |
| MQTT gateway | `cli/src/gateway`, `cli/src/client` | `cli/README.md` |
| Fuzzing | `fuzz/` | `fuzz/README.md` |
