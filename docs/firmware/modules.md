# The Climate, CO2 and Relay Modules

Three TOWER modules that live on the board's I2C bus and need no pin of their own:

| Module | What it is | Driver | In the app |
|--------|------------|--------|------------|
| **Climate Module** | Humidity, illuminance, pressure (and a thermometer) | `tower::climate_module` | `radio_climate` |
| **CO2 Module** | A Senseair LP8 NDIR sensor with its own energy reservoir | `tower::co2_module` | `radio_climate` |
| **Relay Module** | A latching (bistable) relay, one changeover contact | `tower::relay_module` | `radio_control` |

Two rules hold for all three:

- **Nothing has to be fitted.** Every driver starts with a `probe` that says whether its module
  answers; the apps look for a missing module again and again (cheaply: a few addresses that do
  not acknowledge) and drop one that stops answering. A node runs the same with any of them, all
  of them or none.
- **Nothing runs between uses.** Every part converts or switches once when asked and is back in
  its lowest-power state before the driver returns; the MCU sleeps (STOP) through every wait.

The arithmetic, the frames and the pin patterns are in the hardware-free crate
[`tower-modules`](../../crates/modules) (host-tested against datasheet vectors); the drivers in
`firmware/src/` do the bus traffic and the timing.

## What is on the bus

| Module | Part | Address | Part driver | Note |
|--------|------|---------|-------------|------|
| Climate (first revision) | SHT20, humidity | `0x40` | `tower::sht20` | Also the Humidity Tag |
| Climate (first revision) | TMP112, temperature | `0x48` | `tower::tmp112` | Shut down, not read (below) |
| Climate (second revision) | SHT30, humidity and temperature | `0x45` | `tower::sht30` | Not on the bench yet |
| Climate | OPT3001, illuminance | `0x44` | `tower::opt3001` | Also the Lux Meter Tag |
| Climate | MPL3115A2, pressure | `0x60` | `tower::mpl3115a2` | Also the Barometer Tag |
| CO2 | TCA9534A, the power switches and RDY | `0x38` | `tower::tca9534a` | |
| CO2 | SC16IS740, UART to the LP8 | `0x4D` | `tower::sc16is740` | 9600 baud, 8N2 |
| Relay | TCA9534A, the H-bridge and two LEDs | `0x3B` (`0x3F`; a TCA9534 at `0x23`, `0x27`) | `tower::tca9534a` | All four are probed |

The part drivers are generic over `embedded_hal::i2c::I2c`, like `tmp112` and `lis2dh12`. The
module drivers take the board's shared [`I2cBus`](sdk.md) and do each burst of transfers through
its `raw` access with one `rest` at the end, so the board's ATSHA204A is put back to sleep once
per burst with an awaited delay instead of a 3 ms busy-wait after every transfer.

`examples/i2cscan` lists what answers, with the part each address usually is. It probes with a
register pointer write and, failing that, a read: the bare one-byte read it used before missed
the two expanders of the bench stack. The SHT20 answers neither (it takes commands and has
nothing to read before it measured), so `0x40` is probed with its "read user register" command.

## Climate Module (`tower::climate_module`)

```rust
let mut climate = ClimateModule::new(bus);
loop {
    climate.probe().await;            // finds parts not known yet; no traffic when all are known
    let r = climate.measure().await;  // r.humidity, r.illuminance, r.pressure: Option, None = absent
    Timer::after_secs(60).await;
}
```

`measure` starts every conversion at once and collects each when it is due:

| Part | Mode | Ready after | Idle | Per measurement (datasheet) |
|------|------|-------------|------|-----------------------------|
| SHT20 | "no hold master" humidity, 12 bit | 29 ms | 0.15 µA | about 9 µC |
| SHT30 | single shot, high repeatability | 16 ms | 0.2 µA | about 13 µC |
| MPL3115A2 | one shot from standby, 16 samples | 66 ms | 2 µA | about 38 µC |
| OPT3001 | single shot, automatic range, 800 ms | 880 ms | 0.3 µA | about 2 µC |

A round therefore takes about 0.9 s (the light sensor), a few milliseconds of it with the MCU
awake. At one round a minute the parts add about 1 µA to the node's average on top of their
2.5 µA of standby. These are the datasheets' figures: the bench has no PPK2 yet (TASKS.md, T4).

Choices worth knowing:

- **The barometer averages 16 samples**, not the part's 128: about 5 Pa RMS of noise against a
  reported unit of 10 Pa, for a seventh of the charge.
- **The module's own TMP112 is shut down and not read.** It powers up converting four times a
  second (about 10 µA); `probe` puts it into shutdown (1 µA). The Core Module's thermometer, a
  centimetre away, is the `temperature` every taf node reports, so a second one would be the
  same number for more airtime.
- **A part that fails three rounds in a row counts as gone** and is looked for again; a
  barometer found again is set up anew (one that lost its power lost its data-ready flags).
- **The SHT20 does not acknowledge while it converts**: a read that comes too early is a NACK
  and is tried again 10 ms later. An answer of the wrong kind (a stale temperature) or with a
  bad CRC is refused.

## CO2 Module (`tower::co2_module`)

The LP8 is built for batteries: it is powered for one measurement at a time and keeps nothing in
between. The module adds a capacitor that supplies the lamp's pulse (which a battery could not),
and an expander that switches everything:

| Expander pin | Line | |
|--------------|------|-|
| P1 | The capacitor's charging | Made an output (low) to switch on; an input lets the pull-up switch it off |
| P2 | The sensor's supply (VDD2) | Likewise |
| P3 | The sensor's enable | Likewise |
| P6 | The UART bridge's reset | Low pulse at `probe` |
| P7 | RDY from the LP8 | Input |
| P0, P4 | | Always outputs, as the legacy SDK had them |

A measurement is two calls, with a wait the caller is free to sleep through:

```rust
if let Some(mut co2) = Co2Module::probe(bus).await {
    loop {
        Timer::after(co2.charge().await?).await;   // 60 s for an empty capacitor, 5 s for a top-up
        let m = co2.measure(pressure).await?;      // about 0.6 s; everything off again afterwards
        if let Some(ppm) = m.ppm() { /* ... */ }
        Timer::after_secs(300).await;
    }
}
```

What `measure` does, with the LP8 guideline's timing and the legacy SDK's margins:

| Step | Time from power on |
|------|--------------------|
| UART bridge woken and set up, supply and enable on | 0 |
| RDY low (polled every 10 ms from 140 ms) | 136 to 191 ms |
| Write: calculation control, the sensor state (23 B), the pressure; 33 B, 38 ms on the wire | |
| Acknowledgement (4 B) | within 300 ms of the write |
| RDY high: the measurement is done | by about 370 ms |
| Read 44 B from RAM 0x80; the reply is 49 B, 56 ms on the wire | |
| Everything off, the UART bridge asleep | about 0.6 s |

- **The cost is the lamp**: 3.6 mC per measurement by the LP8's datasheet, as much as a few
  hundred rounds of the Climate Module. The average is set by the period: about 12 µA at one
  measurement in 5 minutes, 225 µA at the sensor's minimum of 16 s.
- **Charging**: a minute for a capacitor that may be empty (the first measurement, and after one
  whose status says the capacitor ran short, or that failed), 5 s before every other one, off in
  between. These are the legacy SDK's times.
- **The UART bridge sleeps between measurements** (its oscillator stopped, 30 µA at most by its
  datasheet) and is awake for the exchange: a sleeping SC16IS740 wakes on a start bit but can
  garble that first byte. It is set up again before every measurement, so a module plugged in
  after boot works.
- **The sensor state** (the noise filter and the baseline history) is carried in RAM from each
  reply into the next write. It lasts until a reset; the first measurement after one starts the
  filter over. Every 8 days of uptime the driver asks for an automatic baseline correction (the
  lowest reading since the last one is taken as fresh air, 400 ppm), as the LP8 guideline says.
- **The value** is the filtered, pressure compensated concentration (RAM 0xAA), the one the
  datasheet specifies the accuracy for (±50 ppm ±3 % of the reading). Pass the barometer's
  pressure when there is one; without it the sensor assumes 1012.4 hPa. `Measurement::ppm` is
  `None` when the sensor's own status says the reading is not to be trusted (anything but a
  failed calibration) or it is outside 0 to 10000 ppm.
- **Errors** name the step that failed (`LP8 not ready`, `LP8 did not acknowledge`, ...); the
  module is switched off after every one. Three failures in a row and `gone()` says so.

## Relay Module (`tower::relay_module`)

A latching relay keeps its position without current, so the module costs energy only when it
switches: one 20 ms coil pulse (from the module's reservoir capacitor) and five short bus
transfers. The H-bridge is on the expander's pins:

| Pin | Line | On when |
|-----|------|---------|
| P0, P1 | Green LED (on), red LED (off) | high |
| P4 | `RELAY_F_H`, P-channel high side | low |
| P5 | `RELAY_T_L`, N-channel low side | high |
| P6 | `RELAY_T_H`, P-channel high side | low |
| P7 | `RELAY_F_L`, N-channel low side | high |

"On" is the T direction (the legacy SDK's `true`, the green LED), "off" the F direction.

```rust
if let Some(mut relay) = RelayModule::probe(bus).await {
    relay.set(true, Led::Off).await?;
}
```

- **The coil is never left energised.** The bridge is released after the pulse even when the
  bus fails (three tries); every switch first writes the idle pattern and the pin directions
  (the module may have lost its power since the last one); and `probe` begins by releasing the
  bridge, which also ends a pulse that an MCU reset cut short. A host test checks that no
  pattern turns on both transistors of a bridge leg.
- **The output register is written before the pins become outputs.** It reads `0xFF` after
  power-up, which would turn both low-side transistors on.
- **The position is not readable.** `state()` is `None` until the relay has been switched; a
  `set` pulses the coil also when the relay is already there, which is what makes the position
  known.
- **The LEDs are dark unless asked for** (`Led::State` lights the position's LED): a lit LED
  costs more than the rest of a battery node.
- **Two pulses are at least 300 ms apart**, so the capacitor recharges.

## In the apps

**`radio_climate`** reads the Climate Module in the runtime's sampling round (every
`report-period`, right after the temperature; `taf::round`), so a round's news is one uplink. A
value is news once it moved by its delta since it was last sent: `humidity-delta` (2 %),
`pressure-delta` (1 hPa), `co2-delta` (50 ppm) and `illuminance-delta` (20 % of the reading, 1 lx
at the least); everything goes again with the heartbeat. CO2 is measured every `co2-period`
(300 s) in the first round after it: the charging starts 5 s before that round (a minute before
the first measurement), and the reading is compensated with the round's pressure. `climate print`
(USB console and remote shell) shows the sensors found and their last values. The app is built at
opt-level "z" (the drivers cost 14 KB at "s", which leaves the r1 board 2.4 KB; TASKS.md, D15).

**`radio_control`** looks for the Relay Module at start and every 10 s while it is missing. When
it is found its relay is pulsed to the `relay-boot` state (`off`, `on`, or `last` from EEPROM),
the same setting the Power Module's relay follows. `relay-module` is a control like `relay`
(`nodes/{addr}/set/relay-module`), reported only while the module answers. A click of the Core
button switches the Power Module's relay and the Relay Module's with it; the module's LED shows
the position while the `led` setting is on.

## Bench results

2026-11-10, a Core Module r2 with all three modules, a Core Module r1 with none; the full record
is [results/2026-11-10.md](results/2026-11-10.md).

- **Climate Module**: found in the first round; 36.1 to 36.5 %, 47.9 to 48.6 lx, 949.9 to
  950.3 hPa over an hour in a lit, closed room.
- **CO2 Module**: 695 to 721 ppm in the same room; the first value 67 s after boot (the first
  charge), then one per `co2-period`; no failed or withheld measurement.
- **Relay Module**: found and pulsed to `relay-boot` at start; 42 ms from a click of the Core
  button to the switch (the 20 ms pulse included); a `set` from MQTT confirmed within a second.
- **Without the modules** (the r1): `sensors: none`, the temperature reported as before, no
  warning.
- **Over the radio**: every value on its MQTT `state` topic; with each delta at its minimum one
  uplink per round, carrying what moved.
- **Budget**: `radio_climate` peaks at 5.3 KB of its 10.0 KB of stack (r2, "z") and leaves
  28 KB of flash (14 KB on the r1); `radio_control` peaks at 5.3 KB of 8.2 KB and leaves 2.8 KB
  of flash (its RAM floor is 8.2 KB: 24 B to spare).
- **HIL**: `hil/tests/modules.rs` repeats the first three on the bench (`just hil bench
  modules_`).

## Not verified yet

- **Currents.** Every power figure here is from the datasheets; the idle floor with the modules
  fitted and the charge of a round and of a CO2 measurement need the PPK2 (TASKS.md, T4).
- **The SHT30** (Climate Module, second revision): written from the datasheet and host-tested,
  never on hardware.
- **The relay's contacts**: the expander takes every write of a switch, but nobody was at the
  bench to hear the relay or to measure which contact closes for "on".
- **An automatic baseline correction**: it comes after 8 days of uptime.
