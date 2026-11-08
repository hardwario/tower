# TOWER HMI: the LCD Module and the HMI framework

The SDK drives the **TOWER LCD Module** (a 128 x 128 Sharp memory LCD, two buttons, an RGB LED, a
backlight output and an APDS-9960 gesture sensor) through `tower::lcd`, with an HMI framework on
top: drawing primitives, text in the bundled Spleen bitmap fonts, 1-bit images, declarative menus,
and button and swipe gestures as events. The framework's hardware-free core is the `tower-hmi`
crate (`crates/hmi`, re-exported as `tower::hmi`), host-tested like the other shared crates.

> **Status: implemented and hardware-verified** on an LCD Module rev 2.4 with a Core Module r2:
> the display (drawing, partial updates, rotation), both module buttons and the Core Module
> button, swipes in all four directions, the menu with settings in EEPROM. Chords (both buttons)
> are verified through injected presses only. The power figures below are estimates from the
> datasheets and the code path; they have not been measured on the PPK2 bench yet.

Samples: `lcd_hello` (first light), `lcd_gallery` (the drawing API), `lcd_input` (every input
event, LED feedback, swipes on demand), `lcd_menu` (a settings menu over EEPROM settings),
`lcd_thermometer` (a battery-friendly big-digit readout). `just flash example lcd_hello`.

## Quick start

```rust
use tower::hmi::{Align, Color, TextStyle, fonts};
use tower::lcd::{self, Button, Event, Kind, Rgb};
use tower::{app, board::Board, i2c::I2cBus};

const TITLE: TextStyle = TextStyle::new(&fonts::SPLEEN_12X24).align(Align::Center);

async fn run(b: Board) {
    // The LCD Module shares the I2C bus with the TMP112: turn the bus into a shared handle.
    let bus = I2cBus::share(b.tmp112.release());
    let res = lcd::Resources::new(b.header_spi, b.button, b.header_int);
    let lcd = lcd::init(b.spawner, res, bus, lcd::Config::default()).await.unwrap();

    lcd.draw(|c| {
        c.clear(Color::White);
        c.text(64, 50, "Hello", &TITLE);
    })
    .await;

    loop {
        match lcd.next_event().await {
            Event::Button(Button::Right, Kind::Click) => lcd.led().blink(Rgb::GREEN, 100, 0, 1),
            Event::Button(Button::Left, Kind::Hold) => lcd.set_backlight(true),
            _ => {}
        }
    }
}
app!(run);
```

`lcd::init` returns `Error::NotFound` when no module answers, so an app can run with or without
one.

## Architecture

```
 app ──draw(|canvas| ...)──▶ Lcd ──▶ frame (2 KB, dirty rows) ──flush──▶ SPI2 + DMA ──▶ LS013B7DH03
  ▲                          │                                           (select via the expander)
  └──next_event()──── events ◀── input task: PA8 edge → expander read → Recognizer (tower-hmi)
                             ◀──             PC13 edge → APDS-9960 FIFO → Decoder   (tower-hmi)
     led() / set_backlight() ──▶ service task: LED patterns, VCOM inversion, periodic rewrite
```

* `tower-hmi` (`crates/hmi`, no_std, no dependencies): `Frame` (the 1-bit framebuffer with
  per-row dirty bits), `Canvas` (drawing), `Font` and the Spleen `fonts`, `Image` and `image!`,
  `menu` (the declarative menu model, navigation, rendering), `input` (the button gesture
  recognizer), `gesture` (swipe decoding). Unit- and property-tested on the host (`just test`), and
  `cargo run -p tower-hmi --example preview -- <dir>` renders sample screens to PNG files.
* `tower::lcd`: the module driver. Two tasks: **input** (buttons and swipes into an event channel)
  and **service** (LED timing, VCOM, the periodic rewrite, expander commands). The frame lives in a
  `static` behind an async mutex; `Lcd` is a `Copy` handle usable from any task.
* `tower::i2c::I2cBus`: the board's I2C2 bus as a `Copy` handle shared by the TMP112, the LCD
  Module and anything else on it (see *The I2C bus* below).
* `tower::tca9534a`, `tower::apds9960`: plain drivers over `embedded_hal::i2c::I2c`.

## Hardware (LCD Module rev 2.4)

| Function | Wiring |
|---|---|
| Display | Sharp LS013B7DH03, 128 x 128, on the header SPI: SPI2, SCK PB13 (P14), MOSI PB15 (P13); 1 MHz (the panel's maximum is 1.1 MHz), mode 0, MSB first; DMA1_CH7 |
| Display select | the panel's SCS (active high) = `NOR(PB12, expander P7)`: both low select it |
| Expander | TCA9534A at I2C `0x3C` (A2 strapped high; the schematic's "0x38" is the family base) |
| Expander pins | P0 backlight (low = on), P1 right button, P2 `DISP` (high = shows memory), P3 left button, P4 / P5 / P6 green / red / blue LED (low = on), P7 display select; its INT pin is not connected |
| Buttons | high when pressed (10 kΩ pull-downs); each also drives the header `BOOT` line through a BAV70 diode, which is the Core Module button's net on PA8 |
| RGB LED | six RGB LEDs on common color lines, one resistor each (red 6.8 kΩ, green 10 kΩ, blue 8.2 kΩ): one color for all six, 8 combinations, no dimming |
| Backlight | `BL+` / `BL-` pads, a P-MOSFET and 150 Ω from VDD (for an external backlight) |
| Gesture sensor | APDS-9960 at I2C `0x39`, interrupt (open drain, active low) on the header `INT` line, PC13 |
| VCOM | `EXTMODE` and `EXTCOMIN` tied low: the panel's polarity inversion is the M1 bit of each SPI command |

Two consequences shape the driver:

* **The display select costs 330 µA while asserted**: P7 has a 10 kΩ pull-up (R1) to VDD, so it is
  driven low only for the length of a transfer, and every transfer costs two expander writes.
* **A button press wakes the MCU through PA8**: the expander's interrupt is not wired, but the
  diodes to `BOOT` make PA8 an "any button" line. The driver sleeps on PA8's rising edge and reads
  the expander only while PA8 is high. PA8 high with neither module button down is the Core
  Module's own button (`Button::Core`). As with the Core Module button, holding a module button
  through a reset starts the ROM bootloader.

`Board` hands out the header's bus lines for this: `b.header_spi` (SPI2, PB12..PB15, DMA1_CH6/CH7;
their DMA interrupt shares the console's vector, bound in `ConsoleIrqs`, available to other drivers
as `board::HeaderSpiIrqs`) and `b.header_int` (PC13, left unconfigured until a driver calls
`into_input()`, so an app that doesn't use it leaves the pin in its lowest-power reset state).

## Drawing

`lcd.draw(|c| { ... }).await` locks the frame, runs the closure with a `Canvas`, and sends what
changed. `lcd.update(...)` draws without sending; `lcd.flush()` sends; `lcd.refresh()` rewrites
every row.

* **Colors**: `Color::Black` (ink), `Color::White` (paper), `Color::Invert` (XOR: draws over
  anything and undoes itself when repeated, e.g. a selection highlight with `invert_rect`).
* **Primitives**: `pixel`, `hline`, `vline`, `line` (Bresenham), `rect`, `fill_rect`, `round_rect`,
  `fill_round_rect`, `circle`, `fill_circle`, `invert_rect`, `clear` (fills the clip area). Rounded
  outlines and fills come from the same row spans, so an outline sits exactly on its fill and an
  `Invert` outline strikes no pixel twice.
* **Text**: `c.text(x, y, "text", &style)` and `c.text_fmt(x, y, format_args!(...), &style)` (rendered
  while formatting, no buffer). `TextStyle::new(&FONT)` with `.align(Align::Center)` (or `Right`:
  `x` is then the right edge, exclusive), `.color(...)`, `.background(...)`, `.opaque()` (black on
  white, overwriting what was there), `.inverted()` (white on black). `y` is the top of the line;
  `\n` starts a new line. Both return the area covered. Build styles as `const`s.
* **Images**: `c.image(x, y, &IMAGE, Color::Black, None)`; the last argument paints the image's
  paper pixels too (`Some(Color::White)`) or leaves them (`None`). `image!` builds an `Image` from
  ASCII art at compile time (`#` ink, `.` paper); `Image::new(w, h, &BYTES)` takes the "horizontal,
  1 bit per pixel, MSB first" output of the usual converters (image2cpp, LCD Assistant).
* **Clipping**: `c.set_clip(rect)`, `c.with_clip(rect, |c| ...)`; everything is clipped to it.
* **Rotation**: `Config::rotation` or `lcd.set_rotation(...)` (applied from the next draw; redraw
  the screen after it). `R0` reads with the buttons below the display. Unrotated fills, text and
  images take byte-wide fast paths; a rotated screen draws pixel by pixel (correct, slower).

**Draw immediately, the driver sends the difference.** A canvas marks a row dirty only when a byte
in it actually changes, and the driver keeps a signature (FNV-1a) of each row the panel shows, so
clearing and redrawing a whole screen sends only the rows that came out different: the menu sample
averaged 38 rows per redraw, a changed clock digit is about 20 rows. There is no need to track
what changed in the application.

## Fonts

The bundled font is [Spleen](https://github.com/fcambus/spleen) 2.2.0 by Frederic Cambus,
monospaced, in six sizes:

| Font | Cell | Columns x lines on 128 x 128 | Preset blocks | Flash |
|------|------|---|---|---|
| `SPLEEN_5X8` | 5 x 8 | 25 x 16 | ASCII | 760 B |
| `SPLEEN_6X12` | 6 x 12 | 21 x 10 | ASCII, Latin-1 | 2.2 KiB |
| `SPLEEN_8X16` | 8 x 16 | 16 x 8 | ASCII, Latin-1 | 3.0 KiB |
| `SPLEEN_12X24` | 12 x 24 | 10 x 5 | ASCII, Latin-1 | 9.0 KiB |
| `SPLEEN_16X32` | 16 x 32 | 8 x 4 | ASCII, Latin-1 | 11.9 KiB |
| `SPLEEN_32X64` | 32 x 64 | 4 x 2 | ASCII, Latin-1 | 47.8 KiB |
| `SPLEEN_16X32_NUMERIC` | 16 x 32 | 8 x 4 | numeric | 1.2 KiB |
| `SPLEEN_32X64_NUMERIC` | 32 x 64 | 4 x 2 | numeric | 4.8 KiB |

A font is a list of **blocks**, and the linker keeps a block only if a font the app uses lists it,
so an app pays for exactly the glyphs it can draw. Each size module (`fonts::spleen_8x16`, ...) has
`ASCII`, `LATIN1` (`°`, `µ`, `±`, Western European letters), `LATIN_EXT_A` (Czech, Slovak, Polish,
Hungarian, ...; 8x16 and up), `SYMBOLS` (arrows, block elements for bar graphs, shapes, Greek,
punctuation: whatever the size has) and, for 16x32 and 32x64, `NUMERIC` (space, digits, `%+,-./:`
and `°`). Compose your own:

```rust
use tower::hmi::{Font, fonts::spleen_8x16};
static CZECH: Font = spleen_8x16::with(&[&spleen_8x16::ASCII, &spleen_8x16::LATIN1, &spleen_8x16::LATIN_EXT_A]);
```

A character no block has draws as `?`. Upstream Spleen 5x8 encodes Latin-1 with empty
placeholder glyphs; the generator drops them, so in 5x8 `°` falls back to `?` instead of vanishing.

The glyph data (`crates/hmi/src/fonts/spleen/*.bin`, `src/fonts/spleen.rs`) is generated from the
checksum-pinned release by `crates/hmi/tools/spleen.py`; edit its block table and re-run it to
change what each size carries. **License**: Spleen is BSD-2-Clause (`crates/hmi/LICENSE-spleen`):
a product that ships firmware with these glyphs must reproduce that notice in its documentation.

## Input

`lcd.next_event().await` yields `Event::Button(Button, Kind)` and `Event::Gesture(Gesture)`;
`try_event` and `clear_events` complete the set. The queue holds 16 events; when the app falls
behind, the newest are dropped.

`Button` is `Left`, `Right`, `Both` (the two module buttons as a chord) or `Core`. `Kind` follows
`tower_hmi::input`'s rules, per session (first press until every button is up):

1. `Press` / `Release` for each button as its debounced state changes.
2. `Hold`, once per session, when the buttons down have been held `hold_ms` (800 ms) without
   another joining; two held together give `Hold` of `Both`.
3. `Repeat` every `repeat_ms` after the Hold while the same buttons stay down (off by default).
4. `Click` when the session ends without a Hold within `click_ms` (500 ms); a quick press of both
   buttons gives `Click` of `Both`.
5. `DoubleClick` when `double_click_ms` is set (off by default): a second click of the same
   buttons within it. Each single Click is then held back that long, so enable it only where used.

Timing is `Config::buttons` (debounce 20 ms); `Config::scan_interval` (10 ms) is the sampling
period while a button is down. After a Hold without Repeat the driver samples every 250 ms and
wakes on PA8's falling edge, so a button held down (or stuck) for hours costs no 100 Hz wake-ups.

`lcd.simulate(Button::Left, 100)` (or `lcd::simulate`) injects a press that goes through the same
recognition as a real one.

### Swipes

The APDS-9960 recognizes `Gesture::Up`, `Down`, `Left` and `Right` over the module, in the
module's frame with `Rotation::R0` (verified on hardware: a hand moving from the bottom edge to the
top edge is `Up`). `Config::gesture_map` remaps them for another mounting.

**The sensor is off by default**: with its gesture engine on it never sleeps (about 40 µA in its
wait state, plus the IR LED pulses of each proximity check: roughly 0.3 mA in all at the default
50 ms wait, `apds9960::GestureConfig`). Turn it on while someone is interacting and off after
(`lcd.set_gestures(bool)`); `lcd_input` switches it on at the first button event and off after
30 s without input. The LED boost is off (SparkFun's reference uses 300 %, a current spike that
two AAA cells or a coin cell take badly).

## The RGB LED and the backlight

`lcd.led()` is an `RgbLed` handle with the `led::Led` model: `set(Rgb::RED)` (steady),
`set_background(Some(PATTERN))` (looping), `play(PATTERN)` and `blink(color, on_ms, off_ms,
count)` (one-shot, preempting the background, which then resumes). A pattern is a `&'static
[Step]` of `Step::new(color, ms)`. `Rgb` has the eight on/off combinations. Lit, the six LEDs of a
color draw roughly 0.3 mA (green, blue) to 1.3 mA (red): prefer short flashes to a steady color on
batteries.

`lcd.set_backlight(bool)` switches the `BL+` output; `lcd.set_display(bool)` switches the panel's
`DISP` (off shows white and keeps the memory). All of these are fire-and-forget.

## Menus

Menus are `static` data:

```rust
use tower::hmi::menu::{Item, Menu, MenuState, MenuStyle};

static DISPLAY: Menu = Menu::new("Display", &[
    Item::toggle("Backlight", BACKLIGHT),
    Item::choice("Rotation", ROTATION, &["0°", "90°", "180°", "270°"]),
    Item::back("Back"),
]);
static MAIN: Menu = Menu::new("Settings", &[
    Item::number("Period", PERIOD, 5, 600, 5).unit(" s"),
    Item::number("Alarm", ALARM, -200, 500, 5).decimals(1).unit("°C"), // tenths of a degree
    Item::submenu("Display", &DISPLAY),
    Item::action("About", ABOUT),
]);

let mut menu = MenuState::new(&MAIN);
match lcd.menu(&mut menu, &mut values, &MenuStyle::new(&fonts::SPLEEN_8X16), Some(Duration::from_secs(30))).await {
    MenuExit::Action(ABOUT) => { /* show it, then call lcd.menu again: the state is kept */ }
    MenuExit::Exit | MenuExit::Timeout | MenuExit::Action(_) => {}
}
```

Items: `action` (returns `MenuExit::Action(id)`), `submenu`, `toggle` (0 / 1), `number` (a range, a
step, `.unit(...)`, `.decimals(n)`), `choice` (an index into its options) and `back`. The menu holds
no values: items name `u16` ids and the app passes a `menu::Values` (`get(id)`, `set(id, value)`).
`menu::Table` is a ready-made one; **`shell::SettingsValues`** maps ids onto the app's shell settings
(the ids are the settings' keys), stored in EEPROM as `system settings set` stores them: the device
menu and the console edit the same value. Wrap it to act on a change at once, as `lcd_menu` does
(`set` stores, then switches the backlight or the rotation).

Navigation (`lcd::default_nav`): left click = previous, right click = next (both wrap), right hold
(or a chord click, or the Core button) = enter, left hold = back; swipes up / down move, right
enters, left goes back. In an edited number or choice, previous and next step the value, enter
stores it (`Values::set`), back cancels. `lcd.menu_with(..., nav)` takes another mapping. Without a
`lcd.menu` loop, drive `MenuState::handle(nav, &mut values)` and `MenuState::render(canvas, ...)`
yourself (to keep a live value updating on screen, say).

The renderer draws a title bar (with a back marker in submenus), as many rows as fit, the
selection inverted, values right-aligned (`On` / `Off`, numbers with their unit, choice names, `>`
for submenus), the edited value boxed, and a scroll bar when needed. `MenuStyle` sets the fonts,
padding and row spacing; `render_in` draws into a part of the screen.

## Low power

Nothing polls while the module is idle, and the panel keeps its picture with no refresh. What the
driver still does, and what it costs (estimates; the PPK2 measurement is still to do):

| Activity | When | Cost |
|---|---|---|
| VCOM inversion | every `vcom_period` (1 s) without a flush | two expander writes + a 2-byte SPI command, about 1 ms awake: ~3 µA at 1 Hz |
| Flush | on `draw` / `flush`, changed rows only | DMA at 1 MHz, 18 bytes per row (~150 µs), CPU asleep; plus the two select writes |
| Periodic rewrite | every `refresh_period` (10 min) | one full frame, ~20 ms of DMA: negligible |
| Button press | on each press | PA8 wakes the MCU; the expander is read every 10 ms while a button is down |
| Gesture sensor | only when on | ~0.3 mA (see *Swipes*) |
| RGB LED, backlight | only when on | 0.3 to 1.3 mA per lit color; the backlight depends on what is fitted |

* **VCOM period.** The panel needs its polarity inverted periodically, or a DC bias builds up across
  the liquid crystal (image retention, slow degradation). Sharp characterizes the LS013B7DH03's
  power at 1 Hz inversion, the default here. HARDWARIO's earlier C SDK used 15 s; a longer
  `vcom_period` saves the ~3 µA per toggle per second at the panel's expense. Every flush inverts
  it too, so an updating screen needs no extra commands.
* **The ATSHA204A.** Expander traffic at 100 kHz holds SDA low longer than the ATSHA204A's wake
  threshold, so every burst wakes it (hundreds of µA until put back to sleep). The driver batches
  its expander writes and re-sleeps the chip once per burst through `I2cBus::rest`, which waits out
  the chip's 2.5 ms wake delay with an async timer (the MCU in STOP) instead of the 3 ms busy-wait
  of the per-transaction `AtshaGuard`.
* **Expander writes are cached**: a write that changes nothing is skipped.
* **SPI pins**: SCK idles on embassy's pull-down and MOSI gets one too, so neither floats in STOP
  while the SPI is disabled between transfers.
* **Boot hygiene**: the module keeps its state across an MCU reset. `Board::take` powers down an
  APDS-9960 it can identify by its ID register and, if the expander carries this driver's pin
  directions, releases the display select and turns the panel, LED and backlight off. Any app
  therefore starts without a gesture sensor or a display select left running by the previous one.

## The I2C bus

`Board::take` gives the I2C2 bus to the TMP112 (`b.tmp112`). An app with several drivers on the bus
turns it into an `I2cBus`: `I2cBus::share(b.tmp112.release())`, then `Tmp112::new(bus, ...)`,
`lcd::init(..., bus, ...)`. Through `embedded_hal` it behaves like the guarded bus (each transaction
re-sleeps the ATSHA204A); `bus.raw(|raw| ...)` runs a burst unguarded, followed by one
`bus.rest().await`. Each call locks the bus only for its duration; no lock is held across an
`.await`.

## Shell commands and tools

`lcd::SHELL` is a command subtree to mount (`static CMDS: &[shell::Entry] = &[shell::Entry::menu("lcd",
lcd::SHELL)]; app!(run, commands: CMDS);`):

* `lcd press <left|right|both|core> [ms]`: a press recognized like a real one (console only);
* `lcd swipe <up|down|left|right>`: a decoded swipe (console only);
* `lcd stats`: flushes, rows sent, VCOM-only toggles;
* `lcd dump`: the frame as hex, one row per line.

`python3 tools/lcd_dump.py screen.png` renders `lcd dump` to a PNG: what the firmware drew, without
looking at the module. Together with `lcd press` it drives and checks a UI from the console (how
the samples were verified).

## Budget

The driver costs about 2.9 KB of RAM (the 2 KB frame, 512 B of row signatures, a 146-byte DMA
chunk, the two tasks) and roughly 23 KB of flash for the driver, the canvas, the recognizer and the
shell hooks, plus the fonts used (table above). `lcd_hello` is 130 KB of flash with three font
sizes, against 81 KB for a bare SDK example.

## Not done yet

* PPK2 measurement of the idle floor with the module attached, the VCOM toggle and a flush.
* Physical two-button chords (verified with injected presses).
* The I2C bus stays at 100 kHz: at 400 kHz expander bursts would be shorter and might not wake the
  ATSHA204A at all, but that changes the bus for every device on it and needs the bench.
