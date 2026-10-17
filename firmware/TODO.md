# TODO

Planned work that is decided but not scheduled yet. Keep entries concrete: what, why, and how
we'll know it's done.

## FHSS in the product apps (US 915 compliance)

**Why.** `radio_push_button` and `radio_dongle_gateway` accept `band=us915`, but they run it as
a single fixed channel (~+12 dBm, ~216 kHz). `docs/radio.md` already calls that mode
bench-only: it is not FCC §15.247-compliant (that needs FHSS or ≥500 kHz wideband), and at
this power it also exceeds §15.249. Until this lands, **don't deploy either app on `us915`
outside the bench.** (Decided 2026-10-17: fix by wiring FHSS in, not by gating the band.)

**What exists.** The SDK's FHSS mode is implemented and hardware-verified: `Net::enable_fhss`
(`FhssRole::Master` on the gateway, `FhssRole::Node` on nodes), `fhss_master_tick` /
`fhss_node_tick` + `fhss_send`, the beacon epoch, and the `radio_fhss` / `fhss_compliance`
examples (80 channels, 300 ms slot, 24 s cycle; see *Spectrum-access modes* in
`docs/radio.md`).

**What's needed.**
- Gateway: on `us915`, `enable_fhss(Master, …)` instead of a fixed channel; drive
  `fhss_master_tick` from the main loop alongside `recv()`; keep the EU path unchanged.
- Push-button node: `enable_fhss(Node, …)`, `fhss_node_tick` + `fhss_send` for uplinks,
  and a sleep strategy that still re-acquires sync after STOP (the node is a sleeping sensor;
  re-acquisition cost per wake is the open design question; measure it on the PPK2).
- Pairing, ACK, downlink-queue/remote-shell and RadioStat paths under FHSS (today they assume
  a fixed channel).
- `channel` setting semantics on us915 (ignored or rejected under FHSS) + MQTT/CLI
  `describe` reporting.

**Done when.** Two-board on-air test: push-button ↔ gateway on `us915` deliver confirmed
uplinks and remote-shell downlinks with FHSS locked; `fhss_compliance` evidence (≥50
channels, max per-channel dwell ≪ 0.4 s / 20 s) recorded for the product images; RAM budget
guard still OK; `docs/radio.md` + `docs/gateway.md` updated and the bench-only warning above
removed.

## Provisioned ATSHA204A as a secure element (manufacturing)

**Why.** Every Core Module carries an ATSHA204A (I2C 0x64), which the SDK today only puts to
sleep (`board::atsha_sleep`, the `AtshaGuard` bus wrapper). Provisioned in production, it would
give the radio network two things the MCU alone can't:
- **Device attestation**: during the keyed join (E4, `docs/radio.md`), the gateway sends a
  random challenge and the node answers with the ATSHA's HMAC-SHA256 over it under a secret slot
  key that never leaves the chip, so the host knows it is talking to a genuine, provisioned
  board and not to a clone holding a copied link key.
- **Keys at rest**: the link keys (today plain records in the data EEPROM, protected only by the
  flash readout protection) stored in, or wrapped by, an ATSHA slot, so an EEPROM dump no longer
  yields them.

**What's needed.**
- A manufacturing step that writes a per-device secret into a slot (random per device, or
  diversified from a factory master key and the device identity) and records it host-side for
  the attestation check, then **locks the config zone and the data zone** (one-time, irreversible:
  slot policies must be final before the first production run).
- An SDK driver: wake / command / sleep over the shared I2C2 bus (the wake and sleep handling in
  `board.rs` already exists), the HMAC and nonce commands, CRC-16 framing.
- A keyed-join extension carrying the challenge and the HMAC answer, a host-side verifier, and a
  policy for unprovisioned boards (development kits).
- A power check on the PPK2: every ATSHA command wakes the chip (hundreds of µA while awake).

**Decided (2026-10-17).** The ATSHA204A's 72-bit serial number is **not** used as an identity:
anyone with the board can read it and it proves nothing without a provisioned secret. The device
identity is the MCU's factory unique ID (`board::unique_id`, shown as `uid:` in `/system resource
print`).

**Done when.** A provisioned board keyed-joins with a verified attestation and an unprovisioned
or cloned one is refused (logged), the link keys survive a reboot without being readable from
the EEPROM, and the manufacturing procedure (provision, lock, record) is documented.
