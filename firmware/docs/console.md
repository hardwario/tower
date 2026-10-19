# TOWER Console: user guide

A framed, bidirectional **host ↔ target console** for the TOWER Core Module
(STM32L083CZ). One serial link carries everything: structured **logs**, raw
`print!` output, self-describing **events**, and a RouterOS-style **shell** with
target-authoritative TAB completion and a declarative, EEPROM-backed **settings
framework**. The host side is the **`tower`** CLI/TUI.

> **Status: complete and hardware-verified** on the Radio Dongle (STM32L08x). The
> wire codec (COBS + CRC-32 + postcard), the DMA-backed `Uart` transport (RX on a
> circular DMA buffer) with a low-power-correct `WakeGuard`, the synchronous panic path, log /
> print / event streaming with overflow accounting, chunked shell responses, the
> settings framework (6 kinds, range/enum validation, value completion), the
> app-extensible deep-merge command tree, and the `tower` CLI + TUI are all
> implemented and tested on real hardware. The codec is covered by the protocol crate's host
> tests, including a 5000-iteration bit-flip test (in `crates/protocol`, run with
> `cargo test -p tower-protocol` at the repo root).

This is the standalone reference for using and maintaining the console subsystem:
architecture, wire protocol, the firmware + host APIs, and a worked example per feature.

---

## Two pieces

| Piece | Where | What it is |
|---|---|---|
| **Firmware SDK** | `firmware/`: `src/console.rs`, `src/shell.rs` | the on-MCU console: logging backend, event/shell APIs, the framed UART transport |
| **`tower` host CLI** | `cli/` (package `tower-cli`, binary `tower`) | decodes the framed link on your machine: logs / events / shell / TUI |
| **`tower-protocol`** | `crates/protocol` (shared, `no_std`) | the single source of truth for the wire format, used by *both* ends |

Because `tower-protocol` is shared, the wire format cannot drift between firmware
and host: both ends build the same `crates/protocol` source as a path dependency.

## Hardware

The console owns **USART1** on the Core Module:

| Signal | Pin | Notes |
|---|---|---|
| TX | PA9 | target → host |
| RX | PA10 | host → target (full-duplex; wired through the dongle's USB bridge) |
| Baud | n/a | 115200 **8N1** |

It is **DMA-backed**: TX on `DMA1_CH4`, RX on a circular DMA buffer (`RingBufferedUartRx`)
on `DMA1_CH5`, a different DMA channel group (`DMA1_CHANNEL4_5_6_7`) from the WS2812 LED
strip's `DMA1_CH3` (`DMA1_CHANNEL2_3`), so the strip and the console both use DMA with no
channel or IRQ overlap. The hardware-filled RX ring means a host → target byte is never lost
to receiver overrun while a TX burst or critical section delays the reader.

The link is **always framed** (binary COBS frames). A plain serial terminal (`screen`,
`minicom`, or any raw monitor) shows gibberish; use the `tower` CLI (`tower logs`),
which decodes the frames.

That is the Core Module r2 (and the Radio Dongle), the default build. The first Core Module,
**rev 1.3**, has no USB-UART bridge: build with `--features core-module-r1` and the same framed
console runs over a USB CDC-ACM serial port on the STM32's own USB; see *Core Module rev 1.3
(native USB)* below.

## Quick start

```sh
# Firmware: flash any example (it auto-starts the console).
TOWER_DEVICE=/dev/cu.usbserial-140 just flash example console_demo

# Host: install the CLI once (from the repo root), then stream the device.
just install-cli                              # = cargo install --locked --path cli
tower logs                                    # auto-detects a single USB serial port
tower -d /dev/cu.usbserial-140 logs           # or name the device explicitly
```

Every app gets the console for free: `Board::take` (via the `app!` macro) starts it
and emits a boot `Hello` + banner naming the app. You don't wire anything up.

```rust
#![no_std]
#![no_main]
use tower::{app, board::Board};
use log::info;

async fn run(_b: Board) {
    info!("hello from my app");   // → framed Log on USART1, rendered by `tower logs`
}
app!(run);
```

---

## Architecture

```
 producers                         one writer task                 host
 ─────────                         ───────────────                 ────
 log::{error..trace}!  ┐  postcard
 print! / println!     ├─ payload ─► TX ring ───► seq+frame ──► UartTx (DMA ch4) ═══► tower
 console::event().await┘  (drop-     (1.5 KB of   (COBS+CRC)      (PA9, 115200)
 shell responses          newest)    records)
                                       ▲
 host commands ═► RingBufferedUartRx ──► FrameDecoder ─► shell ─► responses ─┘
 (PA10, DMA ch5 ring)
```

- **Producers** serialize their message on the spot (its postcard payload) and push it into a
  1.5 KB byte ring that stores each message at its real size
  (`tower_protocol::ring::PayloadRing`), so it holds 20 to 30 typical log lines. The `log`
  backend and `print!` are non-blocking and safe from any context: if the ring has no room the
  newest is dropped and a counter bumped. The async producers (`event`, shell responses,
  management and uplink frames) wait for room instead while the console is up.
- **One writer loop** (inside `console::manager`) owns the UART TX. It assigns the
  per-frame **`seq`** at send time (so a gap on the wire means *real* loss, never a queue
  drop), frames the payload, and writes it. Before the next real frame it emits a
  `Dropped{count}` marker if anything was dropped.
- **The codec** (`tower-protocol`) frames every message identically; see *Wire
  protocol*.
- **RX** is read by the same manager and **routed by frame type**: shell frames to the
  shell's channel. The manager owns the whole UART so it can
  be torn down/rebuilt on USB unplug/plug (see *Low power & the dynamic console*).

### Low power & the dynamic (USB-gated) console

On the STM32L0 an **enabled USART holds embassy's STOP refcount**, so a permanently-on
console would keep the low-power executor out of STOP *forever*: an unplugged / battery
node would burn ~3.5 mA (WFI at 16 MHz) instead of idling at µA. The console is therefore
**dynamic**: `console::manager` (spawned by `Board::take`) owns USART1 + PA9/PA10 + the
`VBUS_SENSE` (PA12) EXTI, and gates the whole UART on USB presence:

- **USB present** → builds the DMA-backed `Uart` (RX ring on ch5), runs the writer + the RX frame-router, and
  re-emits a `Hello`. While USB is present the enabled USART keeps the MCU in WFI (not
  STOP), which is what we want: the console/shell stay responsive and the node is
  powered/plugged anyway.
- **USB absent** → **drops** the UART, which disables USART1 and releases the STOP
  refcount, so the executor reaches STOP and the node idles at µA (~32 µA @3 V measured,
  vs 3527 µA with a permanently-on console). It then waits for USB on the **PA12 EXTI
  edge plus a ~500 ms RTC poll**: EXTI works in STOP and brings the console up the instant
  VBUS rises (**no reset**), while the poll is a fallback for a *missed* edge. PA12 is
  driven by the FT231X's **CBUS3** output (a push-pull ~3.3 V logic level, *not* a 5 V
  divider), which asserts only tens of ms after power-up, i.e. after the executor may have
  already armed the edge wait, so relying on the edge alone can hang; the poll closes that
  gap. The periodic wake costs sub-µA (STOP floor unchanged).

RX is owned by the manager and **routed by frame type**: `ShellCommand`/`ShellComplete`
go to the shell: the console copies each one into the `SHELL_RX` channel, and the shell's own
`shell_rx_task` drains it into `shell::dispatch_frame` (`shell::serve`/`serve_ext` store the
command tree + settings and spawn that task, once). So the shell keeps working across
teardown/rebuild without owning the raw RX half, and a slow handler never stalls RX.

**The `WakeGuard`:** even while the console is up, STOP would gate the USART clock and the
writer's awaited TXE interrupt would never fire. The writer holds a `WakeGuard(Stop1)`
**per transmit burst** (STOP → plain WFI so the USART stays clocked and the interrupt
fires), dropped between bursts.

### Dynamic console verification

The dynamic behavior (console live on plug, torn down on unplug, no reset) can only be
verified with real USB. Recommended bench procedure:

**Setup.** Power the board from a **separate supply** (battery / power-profiler), *not*
from USB, so plugging/unplugging USB only toggles `VBUS_SENSE`; it doesn't cut power.
Have the `tower` CLI on PATH and flash a console-exercising firmware (`console_full` is
ideal): `just flash example console_full`. (Flashing is unaffected: the STM32 bootloader
runs before the app's console.)

| Test | Steps | Pass |
|---|---|---|
| **A: console up on USB** | USB plugged, `tower logs` | `Hello` then streaming logs; `tower console` shell responds (`system resource print`, TAB) |
| **B: live plug/unplug** | `tower logs` running + current meter on VDD; unplug USB, then re-plug | On unplug: logs stop, **current drops to µA** (~50 ms). On re-plug: reconnects, fresh `Hello`, logs resume |
| **B (no-reset check)** | across the unplug/replug in B, watch the log **uptime timestamps** | Uptime is **continuous / increasing** (device kept running+sleeping). If it resets to 0 → the device rebooted, i.e. *not* the dynamic path |
| **C: headless boot → plug** | power board with **no USB** (idles at µA), then plug USB | PA12 EXTI (or the ~500 ms poll fallback) wakes the MCU; console appears in `tower logs` within ~½ s |

**If a test fails, where to look:**

- **Console never appears on plug** → PA12 isn't reading logic-high when USB is plugged.
  It's driven by the FT231X CBUS3 push-pull output (see the caveat below), which the
  ~500 ms poll already tolerates being late, so if it *never* appears, scope PA12 on
  plug: it must reach the STM32's V_IH, or the manager never sees "USB present." (Hardware
  issue, e.g. CBUS3 mis-configured in the FTDI EEPROM, not the firmware logic.)
- **Logs appear but garbled** → framing / the statics reused across rebuilds (the TX queue
  `TX`, the RX DMA ring `RX_DMA_BUF`).
- **Shell silent, logs fine** → the RX router (`dispatch_frame`) or `SHELL_PARAMS` not set
  (a `no_shell` app has no shell by design).
- **Current doesn't drop on unplug** → the UART isn't being dropped; check the manager's
  unplug branch (`select3`) fires (VBUS debounce is 50 ms).
- **Host reports a `seq` gap on re-plug** → should not occur: the writer's `seq` resets
  per USB session and `tower` resets its per-link `seq` tracking when it decodes the fresh
  `Hello`.

### Core Module rev 1.3 (native USB)

The first Core Module (BigClown rev 1.x, the 2016 "CORE MODULE rev 1.3" schematic) has no
USB-UART bridge and no NRST/BOOT0 wiring to the host: its micro-USB D-/D+ go straight to the
STM32's USB on **PA11/PA12** (through 33 Ω, with a TPD2E001 ESD array and 47 pF at the
connector), and the board has RESET and BOOT buttons. Everything else (radio, sensors, button
PA8, LED PH1, strip, I2C) is wired as on r2. Build for it with the feature `core-module-r1`:

```sh
TOWER_FEATURES=core-module-r1 just build app radio_push_button
TOWER_FEATURES=core-module-r1 just flash app radio_push_button   # tower flash goes over USB DFU
TOWER_FEATURES=core-module-r1,role-gateway just flash example net_confirmed
```

**The link.** A USB CDC-ACM serial port: VID:PID `0483:5740` (the pair the legacy TOWER SDK used
on this board), manufacturer `HARDWARIO`, product `TOWER Core Module`, serial number
`hio-core-module-r1-<the 96-bit unique ID in hex>`. It shows up as `/dev/ttyACM*` (Linux),
`/dev/cu.usbmodem*` (macOS) or a `COM` port (Windows); the baud rate is ignored. The frames, the
queue, the writer and the shell are the r2 ones (`console::usb` only supplies the link).

**Console up = a program holds the port open.** While the USB device is configured and the host
asserts DTR or RTS (opening a port does on Linux and macOS; `tower` drives both as its run
baseline), the console is up, 100 ms later (an opening program may still be flushing its input:
`tower` clears it 50 ms after setting its lines, which used to swallow the `Hello`), and each
opening starts with a fresh `Hello`. While the port is
closed the queue keeps its oldest lines (the boot backlog reaches you when you open it) and the
newest are dropped. A program that opens the port but stops reading can't stall the app: each
64-byte packet write gives up after 500 ms, the console goes down for the producers until the
host reads again, and the next frame resyncs the decoder with a `0x00`.

**Finding the host without VBUS.** Rev 1.3 has no VBUS line to the MCU, and simply enabling the
USB device is not safe: its D+ pull-up held on with no cable feeds the VBUS net through the ESD
array (whose VCC pin is on VBUS), which can switch the board's power path and assert `BAT_OFF` (a
Battery Module then cuts its cells: a reboot loop). So a session starts only after a **pull-up
probe** has seen a host: every 3 s, with the USB peripheral otherwise off, the transceiver is
powered up, its 1.5 kΩ D+ pull-up goes on for ~1.5 µs, and D+ is read from `USB_FNR.RXDP`. With a
host, its 15 kΩ pull-down only takes D+ to ~3.0 V (reads high; verified on a rev 1.3 board on a
Mac); with no VBUS the ESD diode clamps D+ to under 1 V (reads low). The pulse is shorter than the
2.5 µs a hub needs to register a connect, so a host never sees it. Two "present" probes 50 ms
apart start a session; nobody configuring the device within 3 s (a charger, a sleeping host)
ends it, and the next attempt waits 2 s, doubling to 30 s. Only a configuration counts as a host:
the peripheral reports a bus reset of its own when it is enabled, and a suspend 3 ms later while
the host is still debouncing the attach (seen on hardware, where it first ended every session
after 4 ms). After the configuration, a bus suspend (the cable pulled, the host asleep) ends the
session within a few ms.

**Power.** With no host, the USB peripheral is off and the board STOPs between probes (a wake
and a few µs of CPU every 3 s). During a session the USB clock holds the STOP refcount (WFI, like
the r2 console's UART), the regulator stays in main mode in Sleep (the STOP tuning's `LPSDSR`
would otherwise leave HSI48 and the transceiver on the low-power regulator), and the board runs
from USB (the on-board LDO, with `BAT_OFF` disconnecting a Battery Module). HSI48 is trimmed to
the host's SOF by the CRS, started per session (not through embassy's `sync_from_usb`, which
would hold STOP off for good).

**Resets and flashing.** The r2 board's bridge drives NRST/BOOT0 from RTS/DTR; rev 1.3 emulates
that circuit in firmware from the CDC `SET_CONTROL_LINE_STATE` requests: (RTS, DTR) = (on, off)
held at least 20 ms is "RESET", and on its release the board reboots, into the ROM bootloader if
(off, on) ("BOOT0 high") follows within 30 ms, else into the application. So `tower reset`,
`tower logs --reset`, `tower console --reset` and the bootloader entry of `tower flash` work on
this board too; every reset re-enumerates the USB device (the port disappears and comes back,
and the streaming commands follow it).

Getting into the ROM bootloader from the application **erases the start of the application**
(`sysboot`): the STM32L07x/L08x bootloader, started from software, runs its dual-bank boot check
and jumps straight back to valid code in bank 1 (seen on hardware), and raising BOOT0 through
PA8 (tied to the BOOT net by R3) does not outlast the software reset (C1 on NRST stretches it to
milliseconds). So the board files a request in `.uninit` RAM, resets, erases the first page of
each flash bank before any clock is set up, and jumps; with no valid code to return to, the
bootloader stays and its USB DFU device (`0483:df11`) comes up. The `tower` CLI uses this only
for the commands that rewrite the flash (`flash`, `erase`, `reset --bootloader`, which says so);
`tower chip` asks for the buttons instead. By hand, with nothing erased: hold BOOT, press and
release RESET, release BOOT. The DFU "leave" jumps into the new application; AN3156 (5.5) asks an
application started that way to reset the core first, so every board restarts itself through a
reset when it finds the bootloader's clocks still on (`__pre_init` in `sysboot`).

**No live panic frame.** The panic path's blocking USART write has no USB counterpart (USB needs
the interrupt-driven stack), so a crash is reported from the reset-surviving breadcrumb on the
next boot (`crash` ERROR line, `system crash print`).

**Verified on a rev 1.3 board (2026-10-19, macOS host):** enumeration and the framed console
(`tower logs`, `tower exec`), the probe's "present" side, `tower reset` and `tower logs --reset`
(re-enumeration followed), `tower flash` / `tower erase` both from DFU entered by hand and
automatically over the console (blinky, 94 KB, and radio_push_button, 174 KB, which reaches into
bank 2), the DFU leave into the new application. **Not yet:** the probe's "no VBUS" side and the
battery current unplugged (a battery-powered board: the probe must read low and the board stay
in STOP), a charger and a sleeping host (the back-off, no attach storm), Linux and Windows hosts.

### The panic path

The executor is dead in a panic, so the queue/writer can't run. The panic handler
silences the buffered ISR and **blocking-writes one framed `Log` (level Error)
straight to the USART registers via the PAC**, leading with a `0x00` so any byte still
in the shift register can't prefix and corrupt the frame. `tower logs` shows the panic
message + location like any other error.

It then **resets** (it does not halt). Before resetting it writes the crash text into a
reset-surviving `.uninit` RAM breadcrumb (`crashlog`, zero EEPROM wear); the next boot
re-reports it as a `crash`-module ERROR frame and via `system crash print`, so a
battery node that faults **with USB unplugged** still surfaces its crash, after it has
already recovered. The record is `<message> @ <path>:<line>`: message first and clipped
(never dropped), and a dependency's long absolute `~/.cargo/registry/...` path cut to its
last ~48 bytes; a plain bounded `write!` of the whole panic info used to lose the message
whenever the location alone nearly filled the 192-byte record. Crash loops are bounded by
the bootguard's EEPROM-write backoff, and the run length shows in `system eeprom print`.

**HardFaults** follow the same path (record = faulting PC/LR), with one difference: the
SDK's own `HardFault` entry stub copies PC/LR out of the exception frame and then resets
MSP to the top of the stack region before running the handler, so the ~1 KB of reporting
code never runs on whatever was left of a deep stack.

Two failures can't run a handler at all, so the breadcrumb is written elsewhere:

- **Stack overflow.** With flip-link an overflow pushes SP below the RAM origin; the
  HardFault entry's own exception-frame stacking faults again and the Cortex-M0+ **locks
  up**: no handler runs, and only an armed IWDG resets it. The evidence survives in RAM:
  at the next boot, before re-painting, `stack::paint` reads the previous boot's stack
  paint, and if that boot's deepest reach got within 1 KB of the bottom **and** the reset
  was the IWDG, it files a `stack-overflow` record (*"probable stack overflow: IWDG reset,
  stack reached N B from the bottom (M B used)"*). Without a watchdog a locked-up unit stays
  dead until a power cycle, which loses RAM. Arm the IWDG on field units.
- **Wedge.** When the watchdog's liveness gate withholds the pet (`watchdog::require_checkin`),
  the feeder files a `watchdog` record (*"main loop unresponsive ..."*). It is reported only
  if the reset that followed really was the IWDG, and retracted if the loop recovered first.

`system crash print` shows the kind (`panic` / `hardfault` / `stack-overflow` / `watchdog`)
and the text.

---

## Logging

Use the standard [`log`](https://docs.rs/log) macros. The host adds the timestamp,
color, and columns; the device only sends level + uptime + module + message.

```rust
use log::{error, warn, info, debug, trace};
error!("sensor {} fault: {:?}", id, e);
info!("interval set to {} s", interval);
```

- **Levels:** Error, Warn, Info, Debug, Trace. Default max level is **Trace**; lower
  it at runtime with `log::set_max_level(...)`.
- **`module`** is the last `::` segment of the log target (e.g. `power`).
- **Over-long lines are truncated, never dropped**: a message past `MAX_MSG` (192
  bytes) is clipped at a char boundary (a plain `heapless` write would reject it
  wholesale and log nothing).

### Writing log messages

The SDK, the product apps and the examples follow these rules, so the log reads in one voice;
follow them in your app too:

- **Target = the subject.** `target:` names what the line is about, short and lowercase: an
  SDK subsystem (`boot`, `crash`, `net`, `radio`, `rng`, `storage`, `watchdog`) or a part of the
  app (the push button logs `node`, `button` and `accel`; the gateway `gateway`). The console
  shows `target: message`, so the message never repeats the target. A message may open with a
  sub-subject and a colon when a subsystem logs several kinds of things (`net: pairing: SAS
  123456`).
- **Lowercase, no trailing period.** Words keep their case only when they have one of their own:
  acronyms and part names (ACK, SAS, TRNG, EEPROM, I2C, IWDG, USB, TMP112). No capitals for
  emphasis or state (`PAIRED`, `JOIN`; the verdict words below are the one exception), no
  decorative characters (an ellipsis, arrows, dashes, `***`); unit symbols such as `°C` are fine.
- **The event or the state first, then the facts**, short (aim under 72 characters; the TUI's log
  pane is narrow): `click #3`, `paired with gateway 0x0000ab12, SAS 123456`, `pairing window
  open, 60 s`.
- **Values look the same everywhere:** radio addresses `0x%08x` (as `tower` and the MQTT API
  print them), a unit after a space (`60 s`, `-71 dBm`), an event's sequence number `#n`, a
  SAS as six digits, a count of repeats as `(n since the last report)`.
- **Context in parentheses, briefly:** `(not paired)`, `(pairing)`. A remedy only where the
  user has to act, once, on the line that reports the state, not repeated on every event that
  state affects.
- **Protocol messages by name in words** (`join request`, `reveal`, `confirm`), not their wire
  identifiers.
- **One line per event; a long operation logs its start and its outcome**, not each step.
- **Levels:** `error` when a function is lost (no radio, storage failed, a crash); `warn` when
  something failed or was refused and the device carries on (an undelivered uplink, a refused
  frame, a backoff); `info` for state changes and the events a user acts on; `debug` for
  developer detail. A warning that can repeat is rate-limited and reports its count.
- **Verdicts.** A self-checking example (a known-answer test, an edge test, a bench tool) states
  each result as a verdict: the last word of its line (only context in parentheses may follow),
  one of the capital tokens `PASS`, `FAIL`, `MATCH` (a value equals its reference) and
  `MISMATCH`, the only capital words a message may hold apart from acronyms and part names:
  `aes: FIPS-197 vector MATCH (AES-128 ECB)`, `edge: RX timeout recovery FAIL (7 of 10 back in
  ready)`. It ends with one summary verdict, `all checks PASS` (the phrase the bench waits for)
  or `some checks FAIL`: `edge: frame limits, all checks PASS`. A `PASS` or `MATCH` line is
  `info`, a `FAIL` or `MISMATCH` line `error`.
- **Bursts.** The console queue holds about 1.5 KB of encoded messages (20 to 30 typical
  lines) and the logger cannot wait for room (`log` is synchronous), so a longer back-to-back
  burst loses its newest lines; the host then shows `N log frame(s) dropped`. Await
  `console::flush()` before a line that must arrive (a summary verdict always does) or between
  groups of lines.
- **Lines that tools read are an interface:** the verdicts, `net: pairing: SAS n` and the few
  example lines the bench parses (`pair: paired node 0x…, key …`, `persist: TX counter resumed at
  …`, `confirmed: #n delivered`) are matched by the HIL bench (`hil/tests`) and the hwtest
  scripts (`tools/hwtest`); change them together with their readers.

### Raw text

`print!` / `println!` (from the `tower` crate) send a `Print` message: verbatim text
with no level/timestamp, rendered inline by `tower logs`. `println!` appends `\r\n`.

```rust
use tower::println;
println!("raw line {}", n);
```

### Overflow

If producers outrun the writer, the newest messages are dropped and the host shows a
single marker before the next frame:

```
⚠ 22 log frame(s) dropped (device queue full)
```

No sequence gap is reported, because dropped messages never consumed a `seq`.

---

## Events

Structured, **self-describing** key=value records: the host renders any app's events
with no shared per-app schema. `event()` is `async`: while USB is present it applies
**backpressure** (awaits room in the queue, so a burst is never dropped); while USB is
unplugged there is no writer to drain the queue, so it falls back to **drop-newest with a
count** (surfaced later as the `Dropped` marker) rather than parking the low-power executor
forever on a full queue. Call it from async code.

```rust
use tower::console;
console::event("measurement", &[("temp_c", "2351"), ("rh", "41")]).await;
```

Caps (clipped if exceeded): name ≤ 24 bytes, ≤ **6** fields, each key ≤ 12 / value ≤
20 bytes, sized so a worst-case event always fits one frame. View with `tower events`:

```
12:01:07.245 EVENT measurement  temp_c=2351 rh=41
```

---

## The shell

A RouterOS-style command shell over the same link, with **target-authoritative TAB
completion**: the firmware owns parsing, the command tree, execution **and**
completion, so the host can never suggest something the device won't accept. Opt in by
serving it with the board's EEPROM storage:

```rust
use tower::{app, board::Board, shell};
async fn run(b: Board) {
    shell::serve(b.spawner, b.kv);   // base tree + settings
    // … your app; logs/events keep flowing on the same link …
}
app!(run);
```

Drive it from the host interactively or one-shot:

```sh
tower shell                              # interactive REPL; TAB completes
tower exec "system resource print"      # run one command, print result, exit (scripts/CI)
tower complete "system settings set "   # ask the target what completes here
```

### Built-in commands

| Command | Does |
|---|---|
| `system reboot` | flush the reply, then `SCB::sys_reset()` |
| `system resource print` | firmware name + version, session id, `wire: console 0xXXXX radio 0xXXXX` (the schema tags this image was built with: every console frame carries the first, every radio envelope the second), uptime, `mcu:` (part, clock, LSE RTC, flash / RAM / EEPROM), `uid: XXXXXXXX-XXXXXXXX-XXXXXXXX` (the MCU's 96-bit factory unique ID, `tower::board::unique_id`), `rng:` source (`TRNG` / `SW fallback`) |
| `system stack print` | measured stack high-water (boot-painted): used/free/total vs the budget, plus the previous boot's peak after a software/IWDG reset |
| `system eeprom print` | wear gauge: compaction flips vs budget, live/free bytes, resets |
| `system eeprom wipe confirm` | factory reset: an atomic flip to an empty store (power-loss-safe; the store is frozen until the reboot that follows, also on failure). **Console-only** |
| `system crash print` | the last crash recovered at this boot (from the breadcrumb): panic, HardFault, stack overflow, or watchdog wedge |
| `system settings print` | list every setting and its value |
| `system settings set <name>=<value>` | validate by kind + persist |
| `system settings get <name>` | show value + constraints + default |
| `export` | dump all settings as `system settings set` lines (reproducible config) |

Unknown commands return result code 1. Words are separated by spaces (or tabs): the path into
the tree, the command and its arguments, `system settings set name=value`; there is no leading
`/`, and a value may contain one (`identity=lab/room-1`).

**Console-only commands.** The same command tree also serves the **radio remote shell**
(`shell::stream_line`, e.g. a gateway-queued `NodeCmd::Shell`: anything that can publish a
downlink over MQTT). Operations that are destructive or cut the radio link therefore refuse
remote lines with result **5 (`R_DENIED`)** and run only from the USB console:
`system eeprom wipe` (built with `Entry::cmd_console_only`) and `system settings set` of an
address setting (`Kind::Addr`, i.e. `addr`). Everything else (including `system reboot`,
`system settings print/get`, `export`, and setting non-address values) works over the radio.
App commands opt in with `Entry::cmd_console_only(..)`; a handler that is only partly
sensitive checks `ctx.is_remote()` itself.

A response of any length reaches the host whole: the shell pages it out 256 bytes per handler
run (`shell::RESP_CAP`, the handler re-run with an advancing window, the same on the console and
over the radio) as 192-byte `chunk`/`last` frames that the host reassembles; `export` and
`system settings print` are the long ones.

---

## Settings framework

Settings are **declarative**: an app provides a `&'static [Setting]` table and the
shell derives `print` / `set` / `get` / `export` and completion from it, with no
per-setting code. Each `Setting.key` is a **`u8` local** within the shell's EEPROM
namespace (`NS_SHELL`); the shell prefixes it, so app keys can't collide with other
subsystems. **`0x00..=0x0F` is reserved for the SDK base table** (`identity` = `0x00`,
`addr` = `0x01`, the rest headroom for base growth); app settings start at `0x10`.
The partition is load-bearing: a key collision silently aliases two settings' storage.

```rust
use tower::shell::{Setting, Kind};
static SETTINGS: &[Setting] = &[
    Setting { key: 0x10, name: "interval", kind: Kind::Uint { min: 1, max: 3600 }, default: "30" },
    Setting { key: 0x11, name: "verbose",  kind: Kind::Bool,                       default: "false" },
    Setting { key: 0x12, name: "mode",     kind: Kind::Enum(&["p2p","star","mesh"]),default: "star" },
    Setting { key: 0x13, name: "tx_power", kind: Kind::Int { min: -30, max: 20 },   default: "14" },
];
```

### Kinds

| `Kind` | Accepts | Stored as | Use for |
|---|---|---|---|
| `Str { max }` | 1..=`max` bytes of UTF-8 (`max` ≤ 64) | raw bytes | names, SSIDs, tokens |
| `Uint { min, max }` | decimal `u32` in range | 4 LE bytes | intervals, ports, counts, thresholds |
| `Int { min, max }` | decimal `i32` in range | 4 LE bytes | offsets, tx-power dBm, calibration |
| `Bool` | `true`/`false`, `on`/`off`, `1`/`0` | 1 byte | flags |
| `Enum(&[&str])` | one of the listed values | raw bytes | modes, regions, roles |
| `Addr` | 32-bit hex (`0x1a2b3c4d`), or `auto` / `random` | 4 LE bytes | radio addresses (`0` = auto sentinel) |

- **Ranges/choices are enforced on `set`**; an invalid value returns result 2 and
  prints the constraint:
  ```
  > system settings set interval=99999
  invalid value for interval (uint 1..=3600)
  ```
- **`get` shows the derived metadata:**
  ```
  > system settings get tx_power
  tx_power = -10  [int -30..=20, default 14]
  ```
- **Completion is value-aware:** completing after `=` offers the `Enum` choices or
  `Bool` true/false:
  ```
  > system settings set mode=⇥     →  p2p  star  mesh
  ```
  A completion reply is always **one frame**: the target stops adding candidates before the
  encoded reply would exceed the 247-byte payload (or 16 entries) and sets `more`, so many
  long app setting / enum names can't make TAB time out. The common prefix is cut at a char
  boundary (enum values may be non-ASCII).
- Unset / unreadable / out-of-range stored values fall back to the `default`.

`identity` is just an SDK `Str` setting (key `0x00`, ≤32 chars), nothing special.
`addr` (key `0x01`, `Kind::Addr`, default `auto`) is the SDK's other base setting: the
unit's **32-bit radio address** (the frame-header src/dest, i.e. the net-layer `addr`). `auto`
resolves to the chip-UID-derived address; `random` mints a fresh non-zero one from the
STM32L0 hardware TRNG (`board::rand_u32`); the reply names the source (`ok (0x…, TRNG)`;
`SW fallback` if the TRNG failed to start, also shown as `rng:` in `system resource print`).
`get` shows the *effective* address, so it never lies about what the radio uses. It is
boot-applied (reboot to re-address a live node) and **console-only** (`R_DENIED` over the
radio remote shell: re-addressing would cut the link the command arrived on).

---

## Extending the shell (apps)

`serve_ext` lets an app add its **own commands and settings**. App commands
**deep-merge** with the SDK tree at *every* level, so you can drop a command into an
existing menu *or* grow your own nested subtree. App settings join the same
`system settings` table.

```rust
use core::fmt::Write;
use tower::shell::{self, Args, Ctx, Entry, Kind, Outcome, Setting};

// A handler writes its response via `write!(ctx, …)` and returns an Outcome.
fn cmd_status(ctx: &mut Ctx, _args: &[&str]) -> Outcome {
    let _ = write!(ctx, "radio: idle, last RSSI -71 dBm");
    Outcome::ok()              // or Outcome::code(n) for a non-zero result
}

static APP_COMMANDS: &[Entry] = &[
    Entry::cmd("uptime", Args::None, cmd_status),               // → uptime   (top level)
    Entry::menu("system", &[Entry::cmd("hello", Args::None, cmd_status)]),  // → system hello  (into SDK menu)
    Entry::menu("radio", &[                                     // → radio …  (new subtree)
        Entry::cmd("status", Args::None, cmd_status),
        Entry::menu("test", &[Entry::cmd("ping", Args::None, cmd_status)]),  // → radio test ping
    ]),
];

static APP_SETTINGS: &[Setting] = &[
    Setting { key: 0x10, name: "interval", kind: Kind::Uint { min: 1, max: 3600 }, default: "30" },
];

async fn run(b: Board) {
    shell::serve_ext(b.spawner, b.kv, APP_COMMANDS, APP_SETTINGS);
}
```

The handler context (`Ctx`) exposes:
- `write!(ctx, …)`: `Ctx` implements `core::fmt::Write` (the response text);
- `ctx.kv`: the shell-namespaced (`NS_SHELL`) EEPROM handle; settings are keyed by `u8`
  within it, so use keys other than the SDK base's `0x00` (`identity`);
- `ctx.settings`: the merged settings table (`iter()` / `find(name)`);
- `ctx.is_remote()`: whether the line came over the radio remote shell (answer `R_DENIED`
  for anything that must stay console-only). Whole commands opt in with
  `Entry::cmd_console_only(name, args, handler)` instead.

Merge rules: a menu shadows a same-named command (menus stay descendable); on a
command-name collision the SDK command wins; completion dedups names. Apps don't need
any host changes; `tower` is target-authoritative.

---

## The `tower` host CLI

```
tower [-d <device>] <command>
```

The device auto-detects when exactly one USB serial device is present; otherwise pass
`-d`/`--device`. Subcommands:

| Command | What |
|---|---|
| `devices` | list all serial ports (one bare port name per line) |
| `logs [--color <when>] [--send <text>]` | stream logs + `print!` + the `Dropped` marker; `--send` pokes the device's RX once on connect |
| `events [--color <when>]` | stream structured events |
| `shell` | interactive REPL; TAB completes (via `rustyline`); `exit`/`quit` to leave |
| `exec "<line>"` | run one command, print the (reassembled) response, exit non-zero on a device error/timeout |
| `complete "<line>"` | print what the target would complete at the cursor (handy for testing/scripts) |
| `console` | the full-screen TUI (below) |
| `monitor [--hex]` | transport debugging: dump decoded frames, or every raw byte with `--hex` |

`logs`/`events` auto-reconnect on unplug. Frame-level integrity is reported: a corrupt
frame and a `seq` gap each print a one-line warning.

### The TUI (`tower console`)

A three-pane [ratatui](https://ratatui.rs) terminal app with Device Events, the SSH-style
Interactive Shell (scrollback + a `> ` prompt with in-pane TAB hints) and Device Logs, all
on one serial drain (35/65 split):

```
 HARDWARIO TOWER Console v1.0.0 (/dev/cu.usbserial-140) ●
┌Device Events──────────────┐┌Device Logs────────────────────────────────┐
│12:01 measurement temp=23.5 ││12:01 [  64.030] INFO  app: heartbeat 32    │
│                            ││12:01 [  66.031] WARN  app: link flaky      │
┌Interactive Shell──────────┐│                                            │
│> system settings print    ││                                            │
│identity = node-7           ││                                            │
│> system eeprom print      ││                                            │
│flips: 3 / 100000 (0.0%)    ││                                            │
│> _                         ││                                            │
└────────────────────────────┘└────────────────────────────────────────────┘
 <Shift-Tab> Focus  <F3> Zoom  <F5> Pause  <F8> Clear  <Shift-F8> Clear All …
```

Only the `clock [uptime] LEVEL` prefix of a log line is severity-tinted; command syntax is
highlighted (paths / commands / `key=value`), and `F5` pauses the *view* only; frames keep
being captured while the viewport holds still.

| Key | Action |
|---|---|
| type + Enter | send a shell command (history with ↑/↓) |
| **Tab** | target-authoritative completion (names + enum/bool values) |
| **Shift-Tab** | move focus between panes |
| **F3** | zoom the focused pane full-screen |
| **F5** | pause the streaming panes (shell stays live) |
| **F8** | clear the focused pane |
| **PageUp/Down**, ↑/↓ | scroll a focused stream pane |
| **F10** | quit (restores the terminal; a panic restores it too) |

---

## Wire protocol (`tower-protocol`)

```
wire:   COBS( inner )  0x00
inner:  type(1) │ tag(2, LE) │ seq(2, LE) │ payload(postcard) │ crc32(4, LE)
        type  = the MsgType discriminant
        tag   = CONSOLE_TAG (the console schema's structural hash, folded to 16 bits)
        crc32 = CRC-32/IEEE over [type, tag, seq, payload…]
```

- **COBS** framing with a `0x00` delimiter (the only zero on the wire); the host
  resynchronizes on the next delimiter after any garbage.
- **CRC-32/IEEE** over the header + payload catches corruption (the same primitive
  the EEPROM KV store uses).
- **postcard** for the payload: compact and `no_std`, but **not self-describing**, so
  both ends must share the exact struct/enum definitions. That's why `tower-protocol`
  is one crate that both ends build from the same source.
- **The schema tag** catches the case a build can't: a board or a `tower` binary built from
  another checkout. `CONSOLE_TAG` is derived at compile time from every message type and its
  payload schema (and the management records), so any change that reaches the bytes moves it;
  a CRC-valid frame with another tag is refused (`SchemaMismatch`) before its payload is read.
  It can't say which side is older: the remedy is always to rebuild both from one checkout.
- **`seq`** (writer-assigned) lets the host detect real wire loss.

Message types (the `type` byte; target→host are 0..15, host→target 16+):

| Type | # | Dir | Payload |
|---|---|---|---|
| `Hello` | 0 | T→H | firmware_name, firmware_version, per-boot session_id (boot announce; see below) |
| `Log` | 1 | T→H | level, uptime_us, module, message |
| `Print` | 2 | T→H | raw text |
| `Event` | 3 | T→H | name + (key, value) pairs (wire allows 8; the SDK emits ≤6) |
| `ShellResponse` | 4 | T→H | cmd_id, result, chunk, last, text |
| `ShellCompletions` | 5 | T→H | req_id, token_start, common_prefix, candidates, more |
| `Dropped` | 6 | T→H | count of dropped frames |
| `MgmtResponse` | 7 | T→H | req_id, result, chunk, last, data (chunked mgmt reply; the gateway link) |
| `Uplink` | 8 | T→H | src, counter, rssi, lqi, data (gateway forwards a node's radio payload verbatim) |
| `RadioStat` | 9 | T→H | channel RSSI sample, or a TX-delivery outcome |
| `ShellCommand` | 16 | H→T | cmd_id, line |
| `ShellComplete` | 17 | H→T | req_id, line, cursor |
| `MgmtRequest` | 18 | H→T | req_id, op (a `mgmt::MgmtOp`; the gateway link) |

A receiver feeds bytes to a `FrameDecoder` until it yields a deframed buffer, then
`decode_frame` checks the CRC, then the tag and the type, and returns `(MsgType, seq, payload)`.

**The `Hello`.** The writer synthesizes `Hello` as seq 0 of every console session (boot and
each USB plug-in). Its fields, in wire order:

| Field | Meaning |
|---|---|
| `firmware_name` | the app/example name passed to `app!` (≤ 32 bytes) |
| `firmware_version` | the SDK crate version, `v`-prefixed (e.g. `v0.1.0`) |
| `session_id` | the persisted per-boot counter: same id on a re-plug, new id after a reboot |

The lockstep check is the tag in the `Hello`'s (and every) frame header: a host built from
another checkout refuses the frame (`tower` prints its mismatch banner, naming the firmware read
best effort from that `Hello`, and exits with code 125). The gateway and push-button apps also
return `firmware_version` in their `Describe` reply (`DeviceInfo`), for hosts that attach after
the boot `Hello`. `system resource print` shows the tags this image was built with on its
`wire:` line (`wire: console 0xXXXX radio 0xXXXX`). A stream in which every frame fails its CRC
is a framing difference the tags can't see; `tower` reports that pattern once.

### Limits & constants

| Constant | Value | Meaning |
|---|---|---|
| `CONSOLE_TAG` | (u16) | the console schema tag in every frame header (structural hash, folded) |
| `MAX_FRAME` | 256 | inner frame, pre-COBS |
| `MAX_PAYLOAD` | 247 | postcard payload per frame (`MAX_FRAME` minus the 5-byte header and the CRC) |
| `MAX_WIRE` | 272 | COBS-expanded frame + delimiter |
| `MAX_MSG` | 192 | log / print text |
| `SHELL_CHUNK` | 192 | shell-response text per frame (chunked) |
| `MAX_SETTING` | 64 | largest setting value |
| TX queue | 1536 B | producer → writer byte ring (`console::TX_RING`): messages stored encoded at their real size, 20 to 30 log lines; it replaced 4 slots of ~280 B owned messages, in less RAM |

### Shell result codes

| Code | Name | Meaning |
|---|---|---|
| 0 | `R_OK` | success |
| 1 | `R_NOT_FOUND` | no such command / setting |
| 2 | `R_BAD_ARG` | bad / out-of-range value |
| 3 | `R_STORAGE` | EEPROM write failed |
| 5 | `R_DENIED` | console-only command / address setting refused over the radio remote shell |

---

## Firmware API summary

`tower::console`:
- `event(name, &[(k, v)]).await`: emit a structured event
- `print!` / `println!`: raw text (re-exported macros)
- `install_logger(max_level)`, `manager(usart1, tx_pin, rx_pin, tx_dma, rx_dma, vbus)`,
  `boot_banner(name)`: wiring for the dynamic (USB-gated) console, handled for you by
  `Board::take` / `app!`

`tower::shell`:
- `serve(spawner, kv)` / `serve_ext(spawner, kv, app_commands, app_settings)`
- types: `Entry` (`::cmd` / `::menu`), `Command`, `Args` (`None` / `Names` / `Settings`),
  `Setting`, `Kind`, `SettingsTable`, `Ctx`, `Outcome` (`::ok` / `::code`), `Handler`
- `Entry::cmd_console_only` / `Ctx::is_remote`: keep a command (or part of one) off the radio
  remote shell
- constants: `MAX_SETTING`, `R_OK`/`R_NOT_FOUND`/`R_BAD_ARG`/`R_STORAGE`/`R_DENIED`

`tower_protocol`: `encode_frame`, `decode_frame`, `FrameDecoder`, `MsgType`, the payload
structs, `CONSOLE_TAG`, `radio::RADIO_TAG`, `MAX_FRAME`, `MAX_PAYLOAD`, `MAX_WIRE`,
`crc::{crc32_update, crc32_ieee}`.

## Examples

| Example | Shows |
|---|---|
| `console_demo` | every log level + `println!` + truncation + the `Dropped` marker |
| `console_panic` | the framed panic path (`tower logs` shows the panic record) |
| `events_demo` | structured events interleaved with logs |
| `shell_demo` | the shell: built-ins, settings of every kind, an app command + nested subtree, app settings |
| `console_full` | the showcase for `tower console`: logs + events + shell together |

Flash any of them, e.g. `TOWER_DEVICE=/dev/cu.usbserial-140 just flash example shell_demo`,
then drive with `tower shell` / `tower logs` / `tower console`.

## Testing

The wire codec lives in the shared [`tower-protocol`](../../crates/protocol) crate; its host
tests run in the repo's host workspace:

```sh
cargo test -p tower-protocol   # at the repo root (or `just test` for the whole host workspace)
```

These cover round-trips for every message, boundary frame sizes, the decoder's
overflow/reset/resync state machine, exhaustive type mapping, and a **5000-iteration
deterministic bit-flip fuzz test** proving every single-bit corruption is detected. (They run on
the host; the crate itself is `no_std` and also builds for `thumbv6m`.) The firmware-side paths
(logs, events, shell, settings, completion, panic, persistence) are verified on the dongle.

## Known limitations & caveats

- **DMA:** the console uses DMA: TX on `DMA1_CH4`, RX on a circular DMA buffer on `DMA1_CH5`
  (a different channel group from the WS2812 strip's `DMA1_CH3`, so both coexist). The
  hardware-filled RX ring loses no host → target bytes during a TX flood (an interrupt-per-byte
  RX lost ~1% there). The writer still holds a `WakeGuard` per burst, as belt-and-braces (the RX
  ring already holds the STOP inhibit while the console is up).
- **`fw ?` in the header** appears only if you attach `tower console` and never see a
  `Hello`; the firmware announces it on boot and on each USB plug-in (when the dynamic
  console rebuilds), not on host connect. The TUI
  header otherwise shows the **`tower` CLI** version; the firmware version is in
  `system resource print`.
- **Rev 1.3 (`core-module-r1`):** host detection is the pull-up probe, every 3 s (its "no VBUS"
  side not yet measured on a battery-powered board); every reset re-enumerates the USB port;
  entering the bootloader from the application erases the start of the application; a closed
  port keeps the oldest queued lines; no live panic frame. See *Core Module rev 1.3 (native
  USB)*.
- **`VBUS_SENSE` (PA12):** the dynamic console gates on PA12 reading logic-high when USB
  is plugged. PA12 is driven by the FT231X's **CBUS3** pin (a push-pull ~3.3 V logic
  output configured in the FTDI EEPROM, e.g. `DRIVE1` on the Core Module, `SLEEP#` on the
  Radio Dongle), *not* a resistor divider off USB 5 V. Two consequences: (1) CBUS3 asserts
  only tens of ms after power-up, which is why the manager also **polls VBUS every ~500 ms**
  (a missed rising edge still brings the console up within ½ s); (2) if CBUS3 is
  mis-configured in the EEPROM, PA12 won't reach the STM32's V_IH and the console won't come
  up; scope PA12 on plug to confirm. The pin uses an internal pull-down so it reads a
  defined low when unplugged (no false "present"). PA12 is also the USB DP pin, used here
  purely as a VBUS-sense GPIO (no USB device peripheral is enabled).
- **Setting values can't contain a space or a tab**: whitespace separates the command's words
  (a `/` is fine, e.g. `identity=lab/room-1`). Enum values dodge this by being a fixed set.
- **postcard is not self-describing:** any change to a payload struct/enum is a wire
  change: make it in one commit across `crates/protocol` and its callers (runbook in
  `crates/protocol/AGENTS.md`); there is nothing to bump. Both ends build the change together;
  then reflash the board and reinstall the `tower` CLI, or the stale side's frames carry
  another schema tag and are refused (`SchemaMismatch`).
- **A handler whose output can exceed one 256-byte window runs once per window**: the shell
  pages long responses by re-running the handler, so such a handler must be deterministic and
  free of side effects (the SDK's long ones, `export`, `system settings print` and `system
  resource print`, are read-only; commands that change state print a few bytes and run once).
