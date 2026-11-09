# The PIR Module (`tower::pir`)

The TOWER PIR Module is a motion detector: an Excelitas **PYQ 1648-7053** "LowPower DigiPyro"
behind a Diamond-37 Fresnel lens, and nothing else but its supply filter. The detector is not an
analog pyro element: it carries its own ADC, filters and a motion detection unit, takes its
configuration over one wire (SERIN) and reports over another (DIRECT LINK). Its 1:1 successor,
the **PYQ 1548-7659**, works the same way, and the driver keeps to the timing of both.

| | |
|---|---|
| Detector | Excelitas PYQ 1648-7053: four pyroceramic elements in a TO-5 can, 14-bit sigma-delta ADC, 32 kHz internal clock, 2.5 to 3.6 V, 3 µA typ. |
| SERIN | header **P8 = PB0**, through 470 Ω, 47 pF to ground at the detector |
| DIRECT LINK | header **P9 = PB2** (EXTI line 2), through 470 Ω, 47 pF to ground at the detector |
| Supply | header VDD through 10 Ω, 470 nF |
| Schematic | PIR Module rev 1.5 (`bc-module-pir-rev-1-5-sch.pdf`) |

[`Board::pir`](../../firmware/src/board.rs) hands out the two pins and the EXTI channel; nothing touches them
until an app builds a `Pir`.

## Using it

```rust
use tower::pir::{self, Pir};

let mut pir = Pir::new(b.pir);
pir.configure(&pir::Config::default()).await?;   // write the register, read it back
loop {
    pir.wait_motion().await;                     // the MCU sits in STOP until the detector fires
    info!(target: "pir", "motion");
}
```

- `Pir::new` drives SERIN low (the detector wants it low between writes) and makes DIRECT LINK an
  input with a pull-down. Keep the `Pir` alive.
- `configure` writes the 25-bit configuration register and reads it back: `Err(NotDetected)`
  when nothing answers on P9 (no module), `Err(Readback { .. })` when a different register comes
  back. It also acknowledges a pending event. About 10 ms.
- `wait_motion` sleeps on the EXTI interrupt until DIRECT LINK rises, acknowledges the event
  (which starts the detector's blind time) and returns. Cancel-safe: the detector holds the line
  high until acknowledged, so a `select` that drops it loses nothing.
- `Mode::Signal(source)` instead puts the detector in forced readout: no events, and
  `sample().await` returns the band-pass signal (signed counts of 6.5 µV), the low-pass signal or
  the temperature channel (`None` in motion mode, where the detector allows no readout). The
  `pir_signal` example logs it.

Examples: `pir` (events, the LED, a click steps the sensitivity) and `pir_signal` (the signal).

## How the detector decides

The configuration register, most significant bit first:

| Bits | Field | Meaning | `Config` |
|---|---|---|---|
| 24:17 | threshold | the band-pass level a pulse must cross, in ADC counts (6.5 µV) | `threshold`, 0 to 255 |
| 16:13 | blind time | after an acknowledged event, motion is ignored for 0.5 s + n x 0.5 s | `blind_time`, 0.5 to 8 s |
| 12:11 | pulse counter | pulses that make an event: n + 1 | `pulses`, 1 to 4 |
| 10:9 | window time | the pulses must fall within 2 s + n x 2 s | `window`, 2 to 8 s |
| 8:7 | operation mode | 0 forced readout, 1 interrupt readout, 2 wake-up | `mode` (wake-up or forced) |
| 6:5 | filter source | 0 band pass, 1 low pass, 3 temperature | `Mode::Signal(source)` |
| 4:0 | reserved | must be 16 (on the 1548: factory bits, 0.4 Hz high pass, sign change) | always 16 |

A pulse is the band-pass signal (0.44 to 7 Hz) crossing plus or minus the threshold; the next one
counts only after the signal crossed the other way. A person crossing the lens's zones swings the
signal both ways, a single thermal step (a draught, sunlight on the lens) only one way, so two
pulses reject what one would report. The default `Config` is the STICKER firmware's tested
setting: **threshold 32, 2 pulses within 2 s, blind 1 s** (register `0x402910`). `register()` is a
`const fn`, checked at compile time against the application note's two examples (`0x304D10`,
`0xC01710`) and the STICKER value.

To choose a threshold, run `pir_signal`: it logs the band-pass peak every half second, so the
noise of an empty room and the swing of a person walking past are both on screen.

## Warm-up and settling

A pyroelectric detector needs time after power-up before its filtered signal is quiet. The
detector shares the Core Module's supply, so a power-on reset of the MCU (`RCC_CSR.PORRSTF`) means
the detector just powered up too: `wait_motion` then swallows events for `pir::WARM_UP` (30 s,
as the STICKER firmware does) after boot.

After any other reset (the reset pin, a flash, the watchdog) the detector has been powered all
along and keeps its register (read back intact after a one-minute flash), yet the first write after
the MCU was away swings its band pass: up to 2300 counts after a flash, 1150 after an 8 s reset,
back under 32 within about 4 s. In motion mode that fired a false event 1.85 s after the write,
twice out of two flashes. So events count only `pir::SETTLE` (5 s) after the first successful
`configure`; later writes (a new threshold, a mode switch) were measured not to disturb the
filters and add no wait. `warming_up()` says how long is left of either.

A module plugged in while the board runs is the one case this misses: give it half a minute.

## The two interfaces and their timing

Sources: Excelitas "Device specification for pyro-electric IR-detector PYQ 1648 / 7053" (rev.
27.06.2016) and "LowPower DigiPyro 2nd Gen. Application Note" (rev. 0.61, 16.05.2020, for the
PYQ 15xx). Neither is in the repository (vendor documents).

**SERIN** (configuration in). Per bit, a low to high clock edge, then the bit's value held; after
the last bit, SERIN held low latches the register.

| | PYQ 1648 | PYQ 1548 | driver |
|---|---|---|---|
| clock low / high (t_SL, t_SH) | 200 ns to 2 µs | 200 ns | about 0.9 µs each |
| bit value held (t_SHD) | at least 72 µs | 80 to 150 µs | 100 µs |
| latch (t_SLT) | over 580 µs | over 650 µs | 5 ms |
| DIRECT LINK during the write | not stated | held low | held low |

The 25 bits go out in one critical section (2.5 ms). Split per bit, an interrupt between two bits
could stretch one past the 1548's 150 µs or pause long enough for the detector to latch a partial
register.

**DIRECT LINK** (events and readout). In wake-up mode the detector drives the line high on motion
and keeps it there until the host pulls it low; that acknowledgement starts the blind time. A
readout: the host holds the line high (t_DS), then per bit clocks a low to high edge, releases the
line and samples it once the detector has settled it (the detector's driver is weak: it discharges
the module's 47 pF in about 2.4 µs); the host ends the readout by holding the line low (t_RA).

| | PYQ 1648 | PYQ 1548 | driver |
|---|---|---|---|
| setup pulse (t_DS) | 110 to 150 µs | at least 120 µs | 130 µs |
| clock low / high (t_DL, t_DH) | 200 ns to 2 µs | 200 ns to 2 µs | about 0.9 µs each |
| line low per bit (t_BIT - t_DH) | under 22 µs | under 22 µs | about 6 µs |
| whole frame | under 800 µs | | 0.4 ms (40 bits) |
| end of readout (t_RA) | | at least 160 µs | 200 µs |
| between readouts (t_SMPL) | at least 2.0 ms | at least 2.4 ms | 2.5 ms |
| acknowledge an event | one detector clock | at least 160 µs | 200 µs |

The frame is 40 bits: bit 39 (reserved on the 1648, an active-low out-of-range flag on the 1548),
bits 38:25 the 14-bit signal of the selected source (two's complement for the band pass), bits
24:0 the configuration register. A readout is one critical section (0.4 ms for 40 bits, 0.2 ms for
the 15 signal bits `sample` reads). In wake-up mode the 1548 note allows one readout after a write,
once SERIN and DIRECT LINK have sat low for 2.4 ms; `configure` reads back exactly then, and the
PYQ 1648 answers it too (verified on the bench).

The busy-waits use `cortex_m::asm::delay`, whose `subs` + `bne` loop runs `1 + n / 2` times.
On the Core Module it takes **6 cycles per iteration**, twice the Cortex-M0+ textbook figure, with
the flash at zero wait states and the prefetch on (measured with SysTick: `delay(533)` = 1606
cycles at 16 MHz, 100.4 µs), so `delay(n)` lasts 3n cycles and the SDK's `clock::delay_us` is scaled
to that. The first cut assumed 3 cycles and ran every wait twice as long: the 130 µs setup pulse
became 260 µs, outside the PYQ 1648's 150 µs, and the detector still answered.

## Bench results

Core Module r2 with PIR Module rev 1.5, 2026-11-08:

- Every `configure` (motion, band pass, low pass, temperature, four thresholds) read its register
  back bit for bit; 9.4 ms each.
- Band pass in a still room: peaks of 1 to 7 counts per half second (about 60 µV peak to peak, the
  datasheet's typical noise). A hand swept 10 to 20 cm from the dome: 40 to 230 counts. Low pass at
  rest: 7830 to 8270 counts; temperature: 7490 counts.
- `pir` at the default setting (threshold 32, two pulses in 2 s, blind 1 s): no event in the
  settle time or while the room was still; three hand waves at 20 to 50 cm gave seven events 1.1
  to 1.5 s apart, a walk past at 2 to 3 m nine events over 19 s.
- Leaving a long stretch of wake-up mode for band-pass readout swings the band pass the same way
  as the first write after a reset (2300 counts after a minute, nothing after 4 s), which shows
  in the first seconds of `pir_signal`.

## Power

The detector draws about 3 µA. The MCU sleeps in STOP until DIRECT LINK rises: one EXTI wake per
event, about 0.2 ms awake to acknowledge it. While the line is high the input's pull-down draws
about 80 µA, but only until the acknowledgement. With the module absent and no `Pir` built, both
pins stay analog.

## Earlier drivers

Two HARDWARIO drivers for this detector came before, and this one checks both against the
datasheet:

- **twr-sdk** (`twr_pyq1648.c`, the BigClown/TOWER C SDK): a correct register layout (wake-up
  mode, band pass, one pulse; presets 150/70/20/9 for low to very high), bits of 81 µs with
  interrupts off. It acknowledges an event by switching DIRECT LINK to output low and straight
  back to input, a few µs at most: under the 1548's 160 µs, and marginal against the 1648's one
  detector clock. It polls the line every 100 ms and ignores events for 5 s after start.
- **STICKER** (`app_pyq1648.c`, Zephyr): the default here comes from it (threshold 32, two
  pulses in 2 s, blind 1 s). It encodes a bit as a 95 µs and a 5 µs phase rather than a short
  clock edge then the value, so the clock high (for a 0) and the clock low (after a 1) are 5 µs
  where the specification allows 2 µs; the detector accepts it. It locks interrupts per bit and
  lets them run between bits, where the 1548's 150 µs bit limit can be overrun, and leaves
  DIRECT LINK floating while SERIN is written (the 1548 wants it low). Its 10 ms
  acknowledgement is ample. It polls every 100 ms, waits 30 s after every start (this driver: only
  after a power-on) and never checks that the module is there.

Neither reads the register back, so neither notices a missing module or a corrupted write.
