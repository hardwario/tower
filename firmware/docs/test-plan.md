# HARDWARIO TOWER Firmware SDK: Hardware Test Plan

A **reusable**, end-to-end validation suite for the TOWER firmware SDK (STM32L083CZ +
SPIRIT1). It covers every subsystem (drivers, console/shell, crypto, storage, the full
radio/network stack, and spectrum-access: EU AFA / US FHSS) with concrete
**pass signals** and a catalogue of **edge cases**. Run it whole for a release gate, or pick a
section for a focused regression. Companion guides: `docs/console.md`, `docs/radio.md`.
Host-only unit tests: `just test` (here: the shared crates the firmware builds on; at the repo
root: the whole host workspace).

> This document is intended to be re-run by a human or by an agent. Section 3 explains the
> capture mechanics that make the firmware's "print once at boot" examples observable; the
> harness lives in `tools/hwtest/`.

---

## 1. Scope & philosophy

- **Self-checking first.** Most non-interactive examples are *known-answer tests* (KATs) or
  invariant checkers that log a verdict per check (its last word `PASS` / `MATCH`, or `FAIL` /
  `MISMATCH` on an `error!` line) and one summary verdict, `… all checks PASS` or `… some checks
  FAIL` (the convention in `firmware/AGENTS.md`, "Log messages"). The pass criterion is "the
  summary verdict is present and positive." These need no human judgement.
- **Two-board RF tests** assert link-level behaviour (delivery, ordering, ACK, RSSI, hopping).
  Pass = both ends log the expected exchange with no `FAIL`/`MISMATCH`.
- **Interactive/physical tests** (button, accelerometer dice/tilt, LED visual, LED strip)
  require a human; the plan documents the action and the expected log/visual so they can be
  signed off by eye. An agent without hands records "boot OK + instructions emitted; physical
  step deferred."
- **Regulatory citations are load-bearing.** FCC §15.247 (US FHSS) and EN 300 220 (EU duty /
  LBT+AFA) behaviours are tested by `net_duty_kat`, `fhss_kat`, `fhss_compliance`. Treat a
  failure here as a compliance regression, not a flake.

## 2. Hardware setup & board roster

| Item | Value |
|---|---|
| MCU | STM32L083CZ (Cortex-M0+), chip id `0x447` |
| Target | `thumbv6m-none-eabi` |
| Flash transport | Dongle: STM32 UART bootloader via `tower flash` (tower-flasher engine, ~30 s/image). Core: **SWD via probe-rs** (J-Link), which doesn't need the FTDI, so the Core can be measured with USB unplugged. |
| Console | USART1 PA9/PA10, 115200 8N1, COBS+CRC32+postcard framed (`tower-protocol`) |
| Radio | SPIRIT1 / SPSGRF, SPI1 PB3/PB5/PB4, CS PA15, SDN PB7, nIRQ PA7 |
| Reset/boot | NRST/BOOT0 on FTDI **aux** lines (not DTR/RTS) → opening a port does **not** reset |

**The bench (re-confirm every session; names re-enumerate):**

| Port | Board | Instruments | Role |
|---|---|---|---|
| `/dev/cu.usbserial-2120` | **TOWER Core Module** | SEGGER **J-Link** (SWD flash via `probe-rs`) + Nordic **PPK2** (scriptable current supply/measure) | radio NODE; the **power** target (STOP-floor measurements) |
| `/dev/cu.usbserial-2140` | **TOWER Radio Dongle** | USB-powered (VBUS); flashed via `tower flash` | radio GATEWAY; the **smoke** target (one-shot KAT verdicts) |

Confirm ports with `tower devices`; the automated harness is the HIL crate,
[`hil/`](../../hil) (run `just hil` / `just hil-power` / `just hil-full` there, or `just hil
<recipe>` from the repo root; it builds images from this directory, its sibling `../firmware`).
It reads this roster from its `hil.toml` and re-resolves it at startup, failing fast if a board
is absent. Confirm I2C population by flashing `i2cscan`: with
no sensor module attached, **`thermometer` and `accelerometer` report a sensor-absent error**
(expected on a bare Core, not a fault). A sensor module additionally shows `0x49` (TMP112) and `0x19`
(LIS2DH12).

Default radio role assignment for two-board tests:

- **NODE / sender / peer-a** → `/dev/cu.usbserial-2120` (Core Module)
- **GATEWAY / receiver / peer-b** → `/dev/cu.usbserial-2140` (Radio Dongle)

> **Power measurements need the FTDI UNPLUGGED from the Core.** USB/VBUS present keeps the SDK's
> console alive, which inhibits STOP by design, so a plugged-in board never reaches the µA floor.
> The PPK2 supplies the Core at **1.8 V** (the regulator/brown-out knee, not 3 V) after a
> power-cycle (clears the ~200 µA debug-domain residual a probe leaves); readings are never sampled
> mid-SWD-flash (the PPK2 CDC can desync and report tens of mA of garbage). The HIL harness's
> `just hil-power` test enforces all three, and skips with an "unplug the FTDI" message if the
> console still answers.

## 3. Capture methodology (read this before running)

Two facts shape every observation:

1. **No host handshake.** `Board::take` brings up the console and the app logs immediately.
   A KAT prints its verdict within milliseconds of reset, then idles in `loop { Timer::after_secs(5) }`.
2. **Flash resets into the app at the end** (unless `--no-run`). So by the time a *separate*
   `tower logs` attaches, a one-shot verdict is already gone, unless it resets on attach
   (`tower logs --reset`).

Therefore use **two capture modes**:

### Mode A: `tower logs` (decoded, pretty) for *continuous* output
For examples that keep printing (radio links, net, `console_demo`, `events_demo`, `net_persist`,
reference apps). Attach after flashing; you'll catch the steady-state stream.

```sh
python3 tools/hwtest/cap.py <seconds> tower -d <PORT> logs --color never
```

### Mode B: `tower logs --reset` (decoded, from boot) for *one-shot* output
`tower logs --reset` pulses NRST on the handle it then streams from, and clears the input buffer
while the chip is held in reset, so it catches boot output from the first byte, decoded.

```sh
python3 tools/hwtest/cap.py <seconds> tower -d <PORT> logs --reset --color never > out.txt
grep -E 'PASS|FAIL|MATCH' out.txt   # the verdict lines (MATCH also finds MISMATCH)
```

Use Mode B for: all crypto/frame/duty/fhss KATs, `edge_recovery`, `radio_id/state/regdump`,
`storage`/`net_persist` first-boot lines, `console_panic`. Use a **longer window** for tests that
sweep/iterate before printing: `fhss_sweep` (~80 channel locks), `fhss_compliance` (90 slots ×
300 ms ≈ 27 s), `edge_recovery` (10 × 120 ms timeouts).

### Throughput optimisation
The two ports are independent. **Build sequentially** (shared `target/`, single `firmware.bin`),
**flash/capture concurrently** to halve wall-clock on single-board tests. Build each image to its
own path first:

```sh
tools/hwtest/build.sh <example> /tmp/twr/bin/<example>.bin "<features>"
tower -d /dev/cu.usbserial-120 flash /tmp/twr/bin/A.bin &   # concurrent flashes OK
tower -d /dev/cu.usbserial-140 flash /tmp/twr/bin/B.bin &
```

### Interactive shell tests (no TUI needed)
`shell_demo` and any shell-serving app are fully drivable headless:

```sh
tower -d <PORT> exec "system resource print"
tower -d <PORT> exec "system settings set interval=60"
tower -d <PORT> complete "system settings set m"   # TAB-completion candidates
```

## 4. The role/feature matrix

Radio examples select node/gateway/peer behaviour at **compile time** via cargo features
(`TOWER_FEATURES=...`). Both ends must share band/channel.

> **Important (verified on HW):** every two-board example gates **only on `role-node` vs
> default** (`#[cfg(feature="role-node")]` / `#[cfg(not(...))]`). **`role-gateway` is a no-op**:
> it compiles to the *default* branch. So the recipe is always: **one board built `role-node`,
> the other built with no feature** (or `role-gateway`, equivalently). The receiver/gateway/
> sender/host/master side **is** the default branch. `net_p2p` is the exception (`role-peer-a` /
> `role-peer-b`); `net_star` node B adds `node-2` on top of `role-node`. Which physical role the
> `role-node` build plays differs per example: `net_bulk`/`net_pairing` make `role-node` the
> *active* side (requester/joiner), most others make it the sender.
>
> **Flash sequentially, not concurrently.** Flashing two FTDIs at once over one USB bus is
> unreliable (Write-Memory timeouts, port re-enumeration `-120`→`-2120`). `tools/hwtest/tb.sh`
> flashes node then gateway in series, then captures both concurrently. Re-resolve ports each
> run: `P1=$(ls /dev/cu.usbserial-* | sort | head -1)` etc.
>
> **Capturing the gateway:** with sequential flashing the gateway boots ~38 s after the node;
> a sender/receiver pair is fine (sender keeps transmitting), but for a clean gateway-side log,
> flash the gateway and capture it from its own boot.

| Example | NODE/sender/A feature | GW/receiver/B feature | Boards |
|---|---|---|---|
| radio_id, radio_state, radio_regdump | none | none | 1 |
| radio_cw | `role-node` (TX CW) | *(default)* RX/RSSI | 2 |
| radio_linkdiag | `role-node` (TX) | *(default)* RX | 2 |
| radio_beacon | `role-node` (TX) | *(default)* RX | 2 |
| radio_csma | `role-node` (sender) | *(default)* jammer | 2 |
| radio_sleep | `role-node` (duty-cycled) | *(default)* always-on RX | 2 |
| net_secure_ping | `role-node` | `role-gateway` | 2 |
| net_confirmed | `role-node` | `role-gateway` | 2 |
| net_channel | `role-node` | `role-gateway` | 2 (ch2) |
| net_bulk / net_bulk_stress | `role-node` (requester) | *(default)* sender/server | 2 |
| net_bulk_stream | `role-node` | *(default)* | 2 |
| net_pairing | `role-node` (joiner) | *(default)* host | 2 |
| net_keyed_join | `role-node` (joiner) | *(default)* host | 2 |
| net_star | `role-node` (A) / `role-node,node-2` (B) | *(default)* hub | 2-3 |
| net_p2p | `role-peer-a` | `role-peer-b` | 2 |
| radio_band | `role-node` | `role-gateway` | 2 |
| radio_afa | `role-node` (LBT+agility node) | *(default)* scanner/gateway | 2 |
| radio_fhss | `role-node` (follower node) | *(default)* hop-master | 2 |
| edge_rapid | `role-node` | `role-gateway` checker | 2 |
| radio_interop | `role-node` | `role-gateway` | 2 |
| radio_gateway / radio_node | (separate examples) | | 2 |

KAT/single-board radio examples that need **no** feature and **no** peer: `net_persist`,
`net_duty_kat`, `fhss_kat`, `fhss_sweep`, `fhss_compliance`, `edge_recovery`,
`crypto_*`, `edge_frame_limits`.

---

## 5. Test matrix

Legend for **Pass**: the exact substring to grep in the capture. `*` = mode (A=tower logs,
B=tower logs --reset, H=host, X=interactive, S=shell-exec).

### A. Host-side unit tests (`just test`; mode H)
| ID | What | Pass |
|---|---|---|
| H1 | `tower-kv` codec (46 tests: append/latest-wins, torn-append, a torn tail or corrupt mid-log record never resurfacing, torn-flip, torn-superblock fail-closed, compaction + incremental maintenance, delete / tombstones, wipe (power-loss safe at every write), full, the format of a fresh, foreign or old-layout (`TKV1`) region (empty, at most four word writes, power-loss safe) and an initialized one mounting untouched, key-zero reserved; the word-granular power-loss model: every codec write is whole aligned words, a cut inside any word program of set / delete / the in-line and incremental flips / maintain / wipe / init leaves every key at its value before or after the operation (left as it was, erased or garbled), a torn append never changes an earlier key (the TX watermark at every offset mod 4), a corrupt log end or an old record under the first append is never revived, and wear is at most one erase/write cycle per word every other flip) | `test result: ok. 46 passed` |
| H2 | `tower-radio` (12 tests: duty token-bucket math + FHSS hop permutation + CCM RFC 3610 / key-check-value KAT) | `test result: ok. 12 passed` |
| H3 | `tower-net` (101 tests: replay rule + re-ACK cap, KCV-checked addr-keyed lane binding, TX-counter watermark/fail-closed nonce safety + the single `alloc` allocator (cancellation, persist failure, ceiling), ACK resolution (6-byte ACKs), FHSS beacon-epoch acceptance, pairing admission rules, the pairing timing rules (a handshake must fit the window; the joiner's commit round commits on an ACK, discards after the last attempt and outlasts the gateway's post-confirm stall), the pairing-v3 handshake (RFC 7748 §5.2/§6.1 X25519 vectors, RFC 5869 HKDF, a commitment + transcript + key-schedule + SAS golden from an independent reference, commitment binding, a man in the middle changing its nonce after the node's is refused, keys / tags / SAS bind all four values and both addresses, transcript tamper, low-order points, MITM key substitution, payload sizes), keyed-join predicates + the re-key/lane composition, reserved-address rules, CCM nonce layout/uniqueness) | `test result: ok. 101 passed` |
| H4 | `tower-gateway` (23 tests: registry codec/capacity, insert-only `add_new` for OTA commits, downlink queue) | `test result: ok. 23 passed` |

### B. Board bring-up & peripheral drivers
| ID | Example | Mode | Pass / observable | Edge cases |
|---|---|---|---|---|
| B1 | i2cscan | B | `scan complete, N device(s)`; lists `0x64` (+`0x49`,`0x19` if sensor board) | wrong/missing pull-ups → 0 devices; TMP112 in shutdown still ACKs |
| B2 | blinky | X (visual) | boot banner; LED heartbeat (40 ms on / 1960 off) + double-blink every 5 s preempts it | queue depth 8 overflow drops oldest; zero-duration step must not hang; `set_background(None)` idempotent |
| B3 | button | X | `press/release/click/hold` lines + LED flash on each | click ≤500 ms vs hold ≥1 s boundary; debounce 20 ms; button held at boot; rapid double-tap; bounce on release |
| B4 | thermometer | B | on sensor board: `NN.NN °C` every 2 s; **on these bare boards: `read failed` (NACK), expected** | negative temps sign-extend; wrong strap → persistent NACK; I2C lockup |
| B5 | accelerometer | X+B | on sensor board: `face N up` (1 to 6) per orientation, `tilt detected` on shake (rate-limited 500 ms); **bare boards: WHO_AM_I/absent error** | all 6 faces (opposite faces sum to 7); 45° tilt → no face; sensitivity Ultra vs Low; min_interval throttle |
| B6 | strip | X (visual) | boot banner; scrolling rainbow on WS2812 (needs a strip on PA1) | brightness 0/100/gamma; RGB vs RGBW; zero-length; len>buffer panics; DMA1_CH3 contention |
| B7 | storage | B then reset+B | boot 1: `settings initialized to defaults`, `boot #1`; after reset `boot #2`, `#3`… (counter persists) | missing key→default; bad CRC→default; wrong-type read→None; compaction when full; power-loss mid-write keeps prior value |

### C. Console / shell / events
| ID | Example | Mode | Pass / observable | Edge cases |
|---|---|---|---|---|
| C1 | console_demo | A | logs at ERROR/WARN/INFO/DEBUG/TRACE + `println!`; long line truncated at 192 B on a char boundary; `Dropped{count}` marker where a 60-line burst overflows the 1.5 KB TX queue, right after the last line that arrived | UTF-8 (`°C`) not split at truncation; seq increments (check with `tower monitor`); drop-newest policy |
| C2 | console_panic | B | countdown `alive, deliberate panic in N s`, then one ERROR frame `panicked at … console_panic.rs:LINE` emitted by the PAC-level panic handler, then a RESET: boot banner + `crash` ERROR frame re-reporting the panic from the breadcrumb (`system crash print` agrees) | panic frame leads with `0x00` to flush a partial frame; if console not up, the breadcrumb still records (verify via the post-reset report); message clipped to 192 B; crash-loops by design → bootguard backoff after 8 |
| C3 | events_demo | A (`tower events`) | `EVENT measurement count=… temp_c=… unit=cdeg` each second + periodic `heartbeat`; interleaves with logs in `tower logs` | field cap 6; value/name length clip on char boundary; events apply backpressure (never dropped) |
| C4 | shell_demo | S | `tower exec` of `system resource print`, `system settings print`; set/get all 5 kinds (Str/Uint/Int/Bool/Enum); app cmds merged under `system`; nested `radio …`; `export` | out-of-range rejected (`R_BAD_ARG`); unknown cmd (`R_NOT_FOUND`); TAB ambiguous vs unique (`tower complete`); enum/bool value completion; **persistence across reset**; chunked `resource print` (>192 B → multi-frame); its `uid:` line shows three 8-hex-digit words that differ between boards and match the ID a J-Link reads at 0x1FF80050 / 0x1FF80054 / 0x1FF80064 (`tower logs` of a paired push button shows the same ID in `node: paired with gateway 0x…, SAS … (uid …)`) |
| C5 | console_full | X (TUI) / S | `tower console` shows 3 live panes (Events / Interactive Shell / Logs); or drive shell via `tower exec` | pane focus/zoom/pause; rapid updates no flicker |
| C6 | blinky + a `tower` from another checkout | S / A | the schema mismatch path: with the firmware flashed from this checkout, run a `tower` built from another one (e.g. one installed before a wire change, or this tree with one field appended to `msg::Print`): `tower exec system resource print` exits **125** and stderr carries the `SCHEMA MISMATCH` banner naming both console tags and the firmware (`blinky v0.1.0`) read from its `Hello`; `tower logs` counts every frame as undecodable instead of rendering it. Then the reverse: this `tower` against a board flashed from the other tree, same outcome. `system resource print` (matching pair) shows `wire: console 0x… radio 0x…` equal to the `hello:` line's `schema 0x…` | a `tower` from before the schema tags (a version-byte header) still refuses the new firmware (it reads the type byte as another version) and the new `tower` refuses old firmware (its seq bytes read as a foreign tag); a framing change (simulate with a firmware whose CRC is computed differently) gives one `every frame fails its CRC: likely a different framing …` line, not a banner |

### D. Crypto & frame KATs (single board, no peer; mode B)
| ID | Example | Pass | Vector / edge |
|---|---|---|---|
| D1 | crypto_aes_kat | `FIPS-197 vector MATCH (AES-128 ECB)`; the `got` line reads `69c4e0d8…c55a` | FIPS-197 App. B; mismatch ⇒ key/word/byte order or AES clock |
| D2 | crypto_ccm_kat | `RFC 3610 vector #1 and tamper, all checks PASS`; `tag 17e8d12cfdf926e0 (expected 17e8d12cfdf926e0)` | seal CT+tag `MATCH`; open recovers PT; tampered byte ⇒ `refusal of a tampered ciphertext PASS` |
| D3 | crypto_frame_loopback | `codec, nonce and CCM loopback, all checks PASS` | DATA hdr round-trip + tamper + wrong-key reject; BULK 3-byte index in nonce |
| D4 | edge_frame_limits | `frame limits, all checks PASS` (12 checks; each logs only when it fails, `<check> FAIL`) | MTU 74/64 accept, 75/65 reject `PayloadTooLong`; `BadFormat`/`BadType`/`TooShort`/`AuthFail` |
| D5 | crypto_x25519_kat | `x25519: all checks PASS (…)` (re-emitted every 5 s, so mode A works too) after 11 check lines, 9 ending `MATCH` (RFC 7748 §5.2 vectors 1+2, iterated ×1, §6.1 both public keys + shared secret from both sides, the pairing-v3 commitment golden, the link key + SAS + tags golden) and 2 ending `PASS` (only the revealed nonce opens the commitment; both sides agree); record the `timing:` line (scalar mult ms (1175 measured), commitment ms, key schedule ms, handshake critical path vs the 1500 ms `PEER_KEX_TIME` budget (1185 measured): must say `within`) and the verdict's `stack peak` | ~12 s of blocking compute before the verdict (each scalar multiplication ~1.2 s); a handshake over budget means pairing waits too short: raise `PEER_KEX_TIME`; a golden `MISMATCH` means the on-target SHA-256 / HMAC / HKDF disagree with the host's |

### E. Storage / counter persistence
| ID | Example | Mode | Pass | Edge |
|---|---|---|---|---|
| E1 | storage | B×2 | see B7: boot counter increments across resets | none |
| E2 | net_persist | B×2 | boot 1 `TX counter resumed at 1, reserve watermark 1025`; advance to ~1025; **after reset**: `TX counter resumed at` ≥1025, `reserve watermark 2049` (never reuses) | counter must not start at 0 or go backward; watermark jumps by RESERVE=1024 |

### F. Radio bring-up (single board; mode B)
| ID | Example | Pass | Edge |
|---|---|---|---|
| F1 | radio_id | `device ID MATCH (partnum 0x01, version 0x30: SPIRIT1, part 304)` | all-0x00/0xFF ⇒ SPI/CS/SDN wiring |
| F2 | radio_state | `to ready: state 0x03 MATCH`, `to standby: state 0x40 MATCH`, `nIRQ on ready PASS (…)` / `nIRQ released after the status read` | nIRQ stuck ⇒ PA7/EXTI; state stuck ⇒ wedged |
| F3 | radio_regdump | every register line ends in `MATCH` (SYNT3 is informational); no `MISMATCH` | all 0xFF/0x00 ⇒ SPI read; field mismatch ⇒ config::apply |
| F4 | edge_recovery | `recovery, all checks PASS (state machine never wedged)` after `RX timeout recovery PASS (10 of 10 back in ready)`, `FIFO flush PASS`, `RX to ready cycle PASS`, `device ID read PASS` | SABORT path; flush; chip responsive after abuse |
| F5 | fhss_sweep | `F1 channel lock PASS (all 80 channels)`; reports `retune and lock max … µs` + the recommended guard | band edges 903/926.7 MHz lock; lock-time spread |

### G. Radio RF link (two boards; mode A both ends)
| ID | Example | Pass (node / gateway) | Edge |
|---|---|---|---|
| G1 | radio_cw | TX: `carrier on, state 0x5f (TX, emitting)`; RX RSSI jumps floor (~−110) → carrier (~−40 dBm) | RSSI stuck at floor ⇒ no emission; `synth lock error` ⇒ synth |
| G2 | radio_linkdiag | TX: `#N: FIFO 16 of 16 B, states 03 4f 5f 03, IRQ … (sent), FIFO after 0 B` | FIFO load/drain; IRQ path; state trace |
| G3 | radio_beacon | TX `#N sent`; RX `rx #N, 16 B, … dBm (raw …), PQI …, SQI …, AFC …`; gap-free (no `(n missed)`) | data-rate/deviation/filter; gaps = loss; large AFC = drift |
| G4 | radio_csma | jammer `carrier on` / `carrier off`; sender `not sent, channel busy: CCA backed off` during jam, `sent, channel clear` after | never busy ⇒ CCA broken; backoff jitter 0-100 ms |
| G5 | radio_sleep | node `radio in sleep` / `radio in shutdown`, `woke from … in N µs`; GW monotonic `rx #N` (`re-linked after sleep`) after each | SLEEP wake ≪ SHUTDOWN wake; never wakes ⇒ SDN/RCO |

### H. Network layer (two boards; mode A both ends)
| ID | Example | Pass | Edge |
|---|---|---|---|
| H4 | net_secure_ping | RX `authenticated from 0x…, counter …, … dBm: "ping NNN"`; forged ⇒ `frame dropped: authentication failed` | nonce uniqueness; key mismatch |
| H5 | net_confirmed | node `#N delivered (… ms)`; GW `rx from 0x…, counter …, … dBm: "msg NNN" (ACKed)`; retransmit doesn't double-deliver | `not delivered` storms ⇒ ACK path; replay (cnt≤last_seen) dropped |
| H6 | net_channel | same as H5 but on **ch2** (868.5 MHz); both ends must set CHANNEL=2 | per-channel VCO lock |
| H7 | radio_gateway+radio_node | node `#N delivered (… ms): … mV, … °C`; GW `node 0x… #N: … mV, … °C` | marginal link ⇒ many `not delivered` |

### I. Bulk transfer (two boards; mode A)
| ID | Example | Pass | Edge |
|---|---|---|---|
| I1 | net_bulk | requester `fetched 200 B in 4 chunks, pattern MATCH` | chunk order; nonce per chunk; 30 s session timeout |
| I2 | net_bulk_stream | `round N PASS (… B in … chunks, … ms, … bps, CRC …)` cycling 4/16/32/64 KB (US915, fast) | constant RAM across sizes (streaming); 64 KB exceeds a whole-transfer buffer (~6 KB) |
| I3 | net_bulk_stress | `round N PASS (2048 B in 32 chunks, …)`; later `round N: fetch failed or timed out` as the EU bucket drains | duty enforcement on bulk; CRC-32 |

### J. Pairing & topologies (two boards; mode A)
| ID | Example | Pass | Edge |
|---|---|---|---|
| J1 | net_pairing | host `paired node 0x…, key xxxxxxxx (…)`; joiner `joined host 0x… as 0x…, key xxxxxxxx (…)` with the same 4 key bytes and the same node address; both consoles `net: pairing: SAS dddddd` with the **same** 6 digits; no `does not open its commitment` / `failed key confirmation` line on the joiner | joiner picks own address; no key on air (a sniffer sees only the two public keys, the commitment and the two nonces: five frames, JOIN_REQ / JOIN_RESP / JOIN_NONCE / JOIN_REVEAL / JOIN_CONFIRM); a joiner attempt ~1.5 s unanswered, ~3.8 s for a whole handshake; host refusal lines `net: pairing: join request from 0x… refused: already registered, remove it to re-pair / invalid address / not a pairing v3 request (n since the last report)`, at most one per 10 s (the last one for a joiner flashed from an older checkout); TRNG unavailable → `net: pairing: refused, TRNG unavailable (…)` on either end and no frame sent; 60 s window |
| J1b | net_keyed_join | host `node 0x000000bb joined (keyed, no key on air)`; joiner `joined host 0x22222222` then `uplink delivered` (repeats every ~5 s, each join on a fresh counter) | no window; key never on air; a node under another key: host logs `net: keyed join from 0x… refused: does not open under its registered key`, joiner `keyed join failed: no host answered`; reboot the joiner mid-run: it re-joins (fresh counter across the reboot); a captured JOIN_REQ replayed: host logs `refused: stale counter` and sends nothing; on a gateway app `keyed-join=off`: `refused: keyed joins are switched off` |
| J1c | apps `radio_dongle_gateway` + `radio_push_button` (product OTA pairing; `tower gateway` on the dongle, node console on USB) | `tower nodes add --ota` prints `paired 0x… (SAS dddddd)` with the same 6 digits as the node's `net: pairing: SAS dddddd` and `node: paired with gateway 0x…, SAS dddddd (uid …)`; the gateway logs `gateway: node 0x… paired, SAS dddddd`; the node's first `NodeInfo` arrives as an `Uplink` (auto-named), with no `node: pairing: commit uplink` failure after the first attempt in the common case | **lost confirm**: suppress the JOIN_CONFIRM (e.g. a debug build that skips its TX) → node logs `node: pairing: gateway 0x… did not confirm (no ACK), key discarded, hold the button to retry`, LED back to the unpaired double-blink, gateway window still open, a second hold pairs; **full registry** (16 nodes) → `PairingOpen` answers `FULL`, no node half-paired; **re-pair hold** on a paired node (12 s, or `button simulate 13000`) into an open window without removing it on the gateway → gateway logs `refused: already registered, remove it to re-pair (n since the last report)` at most once per 10 s, node's join times out, the old pairing keeps working; after `tower nodes remove` + a new window the same hold re-pairs; **late JOIN_REQ** in the last ~3.9 s of a window → `refused: the window closes before a handshake could finish`; the `Paired`/`MGMT_TIMEOUT` reaches the host within window + 15 s; no `watchdog: main loop unresponsive` during any of it |
| J2 | net_star | GW `2 peers registered`; `rx node A (0x0a0a0a0a) … "A:nnn"`, `rx node B (0x0b0b0b0b) … "B:nnn"` each under its own key | per-peer key + replay lane; B undecodable under A's key |
| J3 | net_p2p | `A: PING n delivered` and `B: PONG n delivered`; bidirectional | shared LINK_KEY; RX-window timing |

### K. Spectrum access: band / AFA / FHSS
| ID | Example | Mode | Pass | Regulatory |
|---|---|---|---|---|
| K1 | net_duty_kat | B | `time on air and token bucket, all checks PASS` (time on air of 30 B = 18 ms / 96 B = 45 ms; bucket consume/refill/cap) | EN 300 220 1 %/h token bucket |
| K2 | fhss_kat | B | `hop permutation, dwell and beacon, all checks PASS` (F5 permutation, F4 dwell ≤300 ms/20 s, F3 beacon round-trip) | FCC §15.247 dwell + ≥50 ch |
| K3 | fhss_compliance | B (≥30 s) | `F10 compliance, all checks PASS (80 channels, airtime max … of 400 ms)`; `channels used PASS (80 of 80, …)`; `band edges used PASS` | §15.247(a)(1)(i) evidence |
| K4 | radio_band | A (2 brd) | node `now on 868 MHz` / `now on 915 MHz`; GW `rx on 868 MHz … node tag 0` + `rx on 915 MHz … node tag 1` | runtime retune; 915 single-ch is bench-only |
| K5 | radio_afa | A (2 brd) | node `#N delivered on channel X`; channel varies under contention; GW follows | EN 300 220 LBT+AFA, CCA −90 dBm |
| K6 | radio_fhss | A (2 brd) | master `beaconing, slot … on channel …`; node `locked to the hop schedule` then `#N delivered on channel …`; re-sync after master restart | 80-ch, 300 ms slot, 24 s cycle |

### L. Stress / recovery / soak (two boards; mode A)
| ID | Example | Pass | Edge |
|---|---|---|---|
| L1 | edge_rapid | node `send #N: N delivered, 0 not delivered`; GW `… accepted, last counter …, counters monotonic PASS`, never `counters monotonic FAIL` | back-to-back confirms; strict-monotonic accept; no double-accept |
| L2 | radio_interop | GW `invariants PASS (…)`; node logs `seed 0x…` (replayable); invariants: CRC, order, duty, confirm-resolution, oversize-rejected | run long (minutes to hours); a latched failure (`invariants FAIL`) lights the LED solid |

---

## 6. Cross-cutting edge-case catalogue

These are higher-value "be creative" cases to fold into runs as time allows. Many require a code
tweak or fault injection (note them as separate experiments, don't leave the tree dirty):

**Security / crypto**
- Replay: re-send a captured frame (cnt ≤ last_seen) → dropped (logged `net: replay from 0x… dropped: …` for a registered peer, ≤ 1 line / 10 s); forged high counter → CCM-rejected *before* it can poison last_seen.
- Re-ACK cap: re-send a peer's *last* confirmed frame N times → at most 9 ACKs come back (sniff with a third board / `radio_beacon` RX), then silence until the peer's next fresh frame.
- Registered-only gateway: a frame sealed under the gateway's `NetConfig::key` from an unregistered (e.g. just-removed) address → no ACK, no `Uplink` on the host.
- Counter saturation: near 2³²−1 the link fails closed (no nonce reuse / wrap).
- Lost watermark with peer-lane records present → boot `NonceLocked` (TX refused), receive still works (needs a KV fault injection: delete `NS_NET` local 0x00, keep a lane record).
- Lane restore across key change: register a peer, exchange > 32 frames (lane persisted), remove, re-add under a **new** key, factory-reset the peer (counter restarts at 1) → its frames are accepted (fresh lane), not dropped as replays; re-add under the **same** key instead → lane resumes (old captures stay dead).

**Framing / console**
- Oversized log (>192 B) and oversized shell response (>256 B) clip on a char boundary, never mid-UTF-8.
- TX-queue overflow (depth 8) → `Dropped{count}` accurate; producers use drop-newest, events use backpressure.
- Panic with a partial frame in flight → leading `0x00` flushes the host decoder; panic frame stands alone.
- Seq wrap (u16) over a long run → host reports no false gaps.
- CRC corruption / COBS resync: garbage + delimiter → host resyncs on next frame.

**Storage / KV (also covered by host tests H1)**
- Torn append, torn flip, torn superblock-commit → fail-closed, prior keys survive.
- Compaction when half is full; `Error::Full` when live set can't fit a half.
- Wrong-type / corrupt read → falls back to default, never panics.

**Radio / RF**
- RX-completion: `PCKT_FLT_OPTIONS` bit6 must be clear or RX_DATA_READY never fires (regression guard = any working RX test).
- State-machine wedge after RX timeout (F4/edge_recovery): must SABORT→READY.
- CSMA under sustained jam: stays `Busy`, never silently transmits.
- Channel boundaries: ch0/1/2 (EU), all 80 FHSS channels incl. edges.
- Duty exhaustion on EU: repeated bulk pulls → `DutyLimited`, refill over time.
- Sleep/shutdown re-link: wake latency SLEEP ≪ SHUTDOWN; both re-establish the link.
- Pairing window expiry with no joiner; a JOIN_REQ for an already-registered / reserved / host-own address → no JOIN_RESP (host logs `pairing: JOIN_REQ for … refused`); two joiners in one window → each adopts only the JOIN_RESP addressed to it and then only that host's JOIN_REVEAL; a lost JOIN_NONCE → the host's ephemeral is spent, the window stays open and the joiner's next attempt pairs; a JOIN_REVEAL whose nonce does not open the JOIN_RESP's commitment (a man in the middle changing its nonce) → joiner logs `does not open its commitment, ignored` and derives no key from it.
- RX deadline straddle: a frame whose sync lands just before an `rx()` deadline completes (deadline extended once, ≤ ~55 ms) instead of being SABORTed; gateway `RadioStat::Channel` keeps flowing from idle recv-slice timeouts without the old 20 ms `rssi_sample` deaf window.

**Power / low-power**
- USB present → console UART up, so the enabled USART holds embassy's STOP refcount → STOP inhibited (plain Sleep/WFI); console + EXTI live.
- Unplugged → `console::manager` drops the UART → STOP reached when idle (~32 µA @3 V); a PA12 edge or the ~500 ms VBUS poll brings the console back on plug-in. UART flush-on-complete prevents truncated lines (regression: last log line not cut off).

---

## 7. Results log

Record one row per test, per run. Template (`tools/hwtest/RESULTS.md` is generated per session):

```
| ID | Example | Features | Port(s) | Verdict | Evidence (grep'd line) | Notes |
|----|---------|----------|---------|---------|------------------------|-------|
```

Verdict ∈ {PASS, FAIL, SKIP(reason), MANUAL(deferred)}. Keep the raw captures
(`/tmp/twr/results/<id>.txt`) for any FAIL.
