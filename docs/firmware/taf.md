# taf: the TOWER Application Framework

taf is what every standard TOWER radio app is built on, so an app is its business logic and
nothing else: it declares its **properties**, runs a task that measures or switches something,
and leaves the radio, pairing, the console, settings, reports, the remote shell and the clock
to the runtime. Every taf app therefore behaves the same over USB, over the radio and over MQTT.

Three layers:

| Layer | Where | What |
|-------|-------|------|
| The property schema | `crates/protocol/src/props.rs` (`tower_protocol::props`) | Each app kind's property table: the contract between node and host |
| The kernels | `crates/taf` (`tower-taf`, `no_std`, host-tested) | Report policy, wall clock, weekly program, thermostat demand |
| The runtime | `src/taf/` (`tower::taf`) | Radio, pairing, settings, reports, downlinks, remote shell, time sync, watchdog |

The MQTT side of the same properties (topics, payloads, the kinds' tables) is
[`docs/cli/mqtt-api.md`](../cli/mqtt-api.md), *Node properties*.

## The apps

| App | Kind | Class | Hardware | What it does |
|-----|------|-------|----------|--------------|
| `radio_button` | `button` | sleeping | Core Module | Button gestures (press, release, click, hold, each switchable) |
| `radio_climate` | `climate` | sleeping | Core Module | The common sensors only for now (the Climate Module's are still to come) |
| `radio_motion` | `motion` | sleeping | Core Module r2 + PIR Module | Motion events with a holdoff, PIR sensitivity |
| `radio_control` | `control` | listening | Core Module r2 + Power Module + LED strip | The relay (P0) and the strip (every `led_strip` effect) |
| `radio_thermostat` | `thermostat` | sleeping | Core Module | Setpoint, mode, weekly program, heating demand (headless for now, see *RAM*) |

Every one of them also has the common sensors (*Sensors* in the contract below). `radio_control` does not fit a
Core Module r1's flash (its USB console's code): the r1 build is a stub that says so. The others
build for both boards.

## The standard app contract

What every taf app does, so a user meets the same device whatever it measures:

- **Boot**: the SDK's boot banner, then (when paired) an `Info` uplink (firmware name and version,
  the radio class, whether it wants the time) and a full report.
- **Reports**: the app reports values; the runtime sends what is news (any change, or for the
  temperature a change of `report-delta`), batched into one uplink, and everything again every
  `heartbeat` (with an `Info`), settings included.
- **Unpaired**: the LED cue for 180 s after boot; holding the Core button for `hold-time` opens
  over-the-air pairing (the gateway's window open: `tower nodes add --ota`), the SAS logged on
  both ends. A 12 s hold re-pairs a paired node. Cable provisioning over USB (`tower nodes add
  --port`) works for every app.
- **USB console**: logs, the shell and the management channel, always. Every app has
  `node status`, `node pair` (console only) and, when it has the Core button, `button simulate
  <ms>` (console only: a press of that length, for tests without hands).
- **Sensors**: every node reports the Core Module's temperature (`temperature`, sampled every
  `report-period`) and raises a `tilt` event when the board moves (the face up after it settled,
  1 to 6, or 0 while moving; at most one uplink per 10 s, the latest face coalesced). `accel`
  (`off`, `low`, `medium`, `high`; default `medium`) is the tilt threshold and takes effect at once;
  `off` powers the accelerometer down. The runtime owns both chips: `taf_base!` hands it the
  TMP112's bus and the accelerometer's interrupt line, and an app that works with the temperature
  reads the runtime's (`taf::temperature()`).
- **Settings**: the common ones (`heartbeat`, `report-period`, `report-delta`, `led`, `band`,
  `hold-time`, `accel`) plus the kind's, in `system settings` like any SDK setting, derived from the
  property table (no hand-written setting list, no key collisions). A setting other than `band`
  can be set over the radio too; `band` is console only (a wrong band cuts the link).
- **Downlinks**: `Set` (a control or a setting; a setting is stored before the app hears of it),
  `Time`, `Describe` (the bridge asks a node it does not know yet) and remote shell lines. Every
  set is confirmed by a report, also one that changed nothing.
- **Classes**: a **sleeping** node wakes the radio only to send, and hears its downlinks in the
  second after each uplink; a **listening** node (mains powered) keeps receiving in 50 ms slices,
  and the gateway sends it a queued command at once. EU868 only for now: a listening node does not
  follow the US915 hop schedule yet, and one fixed channel at full power is not allowed there
  (FCC §15.247), so a listening kind refuses US915 (no pairing, no provisioning, radio off).
- **Time**: a kind with `time` asks the host for the wall clock in its `Info` (at boot and every 12
  hours); the runtime keeps it as an offset over the monotonic clock (never touching the RTC
  calendar, which the radio's hopping depends on).
- **Watchdog**: the runtime feeds it; a wedged node resets.

## Writing an app

1. **Declare the kind** in `crates/protocol/src/props.rs`: a `pub mod <kind>` with its property
   ids (from `APP_BASE`; the common ones use the ids below), its tables of enum names and a
   `KIND`, then add it to `KINDS`. Per property: a name (the MQTT topic leaf and the shell
   setting's name), a type (`Bool`, `Number { min, max, decimals, unit }`, `Enum(names)`), an
   access (`State`, `Control`, `Event`, `Param`, `LocalParam`) and, for a setting, a default.
   The kind's `class` (sleeping or listening) and `time` (it needs the wall clock) go with it.
   The tables are folded into `RADIO_TAG`: rebuild every end and reinstall the CLI.
2. **Write the app** in `apps/radio_<kind>.rs` (the firmware name is how the bridge finds the
   kind). In outline (`apps/radio_motion.rs` is the full one):

   ```rust
   const KIND: props::Kind = props::motion::KIND;
   static SETTINGS: [shell::Setting; taf::settings_len(&KIND)] = taf::settings(&KIND);
   static COMMANDS: &[shell::Entry] = &[taf::NODE_COMMANDS, taf::BUTTON_COMMANDS];

   #[embassy_executor::task]
   async fn detector(mut pir: Pir) {
       loop {
           match select(pir.wait_motion(), taf::next_set()).await {
               Either::First(()) => taf::event(motion::MOTION, 1),
               Either::Second((id, _)) => { /* a setting changed: re-read it with taf::param */ }
           }
       }
   }

   async fn run(b: Board) {
       b.spawner.spawn(detector(Pir::new(b.pir)).unwrap());
       taf::run(tower::taf_base!(b), &KIND, &SETTINGS).await
   }
   app!(run, commands: COMMANDS, settings: &SETTINGS);
   ```

3. **List it**: a `[[bin]]` in `Cargo.toml`, the `apps` and `ram-budget` lists of the `justfile`
   (`just lists` checks the first), and CI's RAM list.

### The runtime's API

| Call | What |
|------|------|
| `taf::run(base, &KIND, &SETTINGS)` | The runtime; never returns. `tower::taf_base!(b)` moves what it needs out of the `Board` (the radio, the LED, the Core button, the TMP112's I²C bus, shared as `base.i2c` for app drivers on it, and the accelerometer's interrupt); `taf_base!(b, no_button)` leaves the Core button to the app (an LCD Module shares its line; pair with `request_pairing` then) |
| `taf::report(id, value)` | A state's or control's current value; cheap, the runtime decides when it goes up |
| `taf::event(id, value)` | Something happened; up to four wait for the radio, more are dropped |
| `taf::next_set().await` | The next `set` of a control (apply it, then report it), or the notice that a setting changed. Wait on it in the app's loop: the runtime confirms a set when the app comes back for the next one |
| `taf::next_button().await` | Core button events the runtime did not spend on pairing |
| `taf::param(id)` | A setting's value (its default when unset) |
| `taf::led()`, `taf::led_on()` | The board LED, and whether the `led` setting allows event feedback |
| `taf::local_time()` | Local wall-clock time (`weekday`, `minute`, ...) once the host synced it |
| `taf::clock_synced().await` | Wakes when the clock is synced (a scheduler looks again) |
| `taf::request_pairing()`, `taf::paired()` | Start OTA pairing from an app gesture; whether paired |
| `taf::temperature()` | The Core Module's temperature (centi °C) as the runtime last read it; `None` before the first read |
| `taf::temperature_sampled().await` | Wakes with each new temperature sample (one waiter: the app) |
| `taf::NODE_COMMANDS`, `taf::BUTTON_COMMANDS` | The standard shell commands, for the app's `COMMANDS` |

### EEPROM keys

Settings live in the shell's namespace at `0x20 + property id` (derived, so they cannot
collide). An app's own records go in `NS_APP`, whose locals are allocated here:

| `NS_APP` local | Owner |
|----------------|-------|
| `0x00` | The taf runtime (the node's provisioning: its gateway and key) |
| `0x10`, `0x11` | `radio_thermostat` (the weekly program; the mode and the manual setpoint) |
| `0x40` | `tower::strip_player` (the strip's state, `led_strip` and `radio_control`) |
| `0x41` | `radio_control` (the relay's last state, for `relay-boot last`) |

## RAM

A taf node runs in the Core Module's 20 KB: about 10 KB of statics for a sleeping node (the
runtime's future with the radio state is about 2.6 KB of it, the console about 3 KB, the shell
1 KB), leaving 10 KB of stack, above the 8 KB floor of `tools/check_ram_budget.py`. Measured
stack peaks (`system stack print`, 2026-11-09, after pairing, remote shell, sets and EEPROM
writes): 4.7 KB `radio_motion`, 5.2 KB `radio_control`, 6.2 KB `radio_thermostat` (6.1 KB on the
r1). So the floor is real margin, not slack. What keeps an app small:

- **A task argument is stored twice** in its future (the argument and the local it moves into):
  build big things (a strip, a driver) inside the task.
- **A value moved into an `async fn` is stored again** in its future; pass references to what the
  caller holds (the console's `enqueue` takes the encoded message by reference for this reason).
- **Nested awaits add up**: a future is as big as its largest await chain. The runtime serves a
  downlink or a pairing from its loop's top level (flags), not from inside the send that
  received it.
- **Fewer tasks**: a task's future is resident for good. `console::event_now` sends an event
  without a waiting future. The runtime's sensor task is 200 B.

**Flash is as tight as RAM for two builds** (2026-11-09, after the sensors moved into the
runtime): `radio_control` on the r2 has 924 B of its 192 KB left, `radio_thermostat` on the r1
268 B. The runtime is in every app, so code added to it
lands in both; check `just fw build app <name>` (and with `TOWER_FEATURES=core-module-r1`)
before and after (TASKS.md, T18).

**The LCD Module does not fit next to the radio yet.** Its driver keeps the 2 KB frame, 512 B of
row signatures and its tasks resident (about 4.3 KB), which leaves a radio node 5.9 KB of stack,
below the measured 6.2 KB peak. `radio_thermostat` therefore runs headless for now. The way out
is a frame-less (page mode) LCD driver: the app draws the screen once per band of rows into a
small buffer that is sent and reused, which costs CPU time, not RAM.
