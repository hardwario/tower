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
