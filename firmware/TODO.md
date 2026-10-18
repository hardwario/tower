# TODO

Planned work that is decided but not scheduled yet. Keep entries concrete: what, why, and how
we'll know it's done.

## FHSS in the product apps (US 915 compliance): remaining verification

**Done 2026-10-18.** `radio_dongle_gateway` and `radio_push_button` hop on `us915` (FCC
§15.247): the gateway is the hop master, the node follows the schedule with every send, pairs on
the rendezvous carrier at low power (§15.249) and re-acquires after a sleep by prediction on the
crystal clock (`clock::now_ms`; embassy's `Instant` runs on HSI16, ±1 %). docs/radio.md
(*Spectrum-access modes*, "In the product apps") and docs/gateway.md (*US 915*) describe it.
Bench (Radio Dongle + two Core Modules, `us915`): OTA pairing of both nodes at low power;
clicks delivered within one slot; remote shell downlink in the uplink's slot, the reply one chunk
per slot; both nodes booting at once (a collision, then the random retry backoff); the gateway
beaconing every slot (all 80 channels, even per-channel airtime in `gateway status`).

**Still to verify** (needs the PPK2 and a lab, not the code):
- The receive cost of re-acquisition (a predicted beacon catch after a sleep, the 15 min resync,
  the 24-48 s rescan after pairing or a reboot) and the STOP floor with the RTC sub-second counter
  at 1024 Hz (`board::init`; the asynchronous prescaler dropped from 127 to 31).
- The EIRP of `config::PA_INDEX_LOW` (about -6 dBm estimated from the PA ramp) against §15.249,
  with the product antenna.
- The §15.247 numbers (channel count, dwell, bandwidth) against the current FCC KDB, with an
  accredited lab, before any product claim. Until then: no US deployment claims.

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
identity is the MCU's factory unique ID (`board::unique_id`, shown as `uid:` in `system resource
print`).

**Done when.** A provisioned board keyed-joins with a verified attestation and an unprovisioned
or cloned one is refused (logged), the link keys survive a reboot without being readable from
the EEPROM, and the manufacturing procedure (provision, lock, record) is documented.

## STOP-mode regulator bits after every wake (power)

**Why.** `board::apply_stop_tuning` sets `PWR_CR.LPSDSR` (low-power regulator in deep sleep) and
`ULP` (VREFINT off) for the measured ~5 µA STOP floor. embassy's L0 `exit_stop` re-inits the
RCC on every wake from STOP, and `rcc/l.rs` writes the whole `PWR_CR` (`write`, not `modify`),
which clears both bits. Only the console manager's 500 ms VBUS poll re-applies them, so after
any other wake (watchdog feeder, temperature read, LED pattern, the 5 ms button poll, a radio
IRQ) the next STOP runs on the main regulator until the next poll: about 2 % of the idle time on
a quiet node, nearly all of it while an LED pattern runs. (Review of 2026-10-17; the share is
arithmetic, not a measurement.)

**Why it is not a one-line fix.** A hook that re-applies the bits before every sleep (embassy-
executor's `trace` feature calls `_embassy_trace_executor_idle` right before the low-power
executor sleeps) would also set LPSDSR when embassy decides on plain Sleep instead of STOP (a
peripheral holds STOP off, e.g. the console UART with USB attached). LPSDSR also puts the
regulator in low-power mode in Sleep (RM0367 PWR_CR), which at the 16 MHz HSI16 clock is out of
spec. The same applies, rarely, to the current poll-based scheme: after a poll wake, an idle
pass that cannot STOP sleeps with LPSDSR set. ULP without FWU also lengthens the wake by the
VREFINT start-up; ST's low-power examples pair ULP with FWU.

**What's needed.** Set the bits on the STOP path only: upstream in embassy (L0 `enter_stop` sets
LPSDSR + ULP + FWU and `exit_stop` clears LPSDSR; `rcc/l.rs` modifies `PWR_CR` instead of
writing it), carried here with a `[patch.crates-io]` until it is released; then drop the console
manager's re-apply.

**Done when.** PPK2: a node with an LED background pattern and a 1 s timer task idles at the
same floor as `lowpower`; with USB attached the board runs with LPSDSR clear; the radio KATs and
a two-board confirmed uplink with a downlink still pass.
