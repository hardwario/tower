# TOWER LED strip: driver, effects and the `led_strip` app

The SDK drives addressable LED strips (WS2812B RGB and SK6812 RGBW, such as the 144-pixel RGBW
TOWER LED strip) on **PA1** through `tower::strip`: a frame of colours you compose with the
effects, rendered over the `tower::ws2812` timer + DMA driver. The hardware-free core (the frame,
the effects, colours and palettes, the render pipeline) is the `tower-strip` crate
(`crates/strip`, re-exported as `tower::strip`), host-tested like the other shared crates.

> **Status: implemented; partly hardware-verified** on the 144-pixel SK6812 RGBW strip with a
> Core Module r2. Verified by eye: the byte order (one pixel of each of R, G, B, W run along the
> strip), the first effects tour, the gamma curves and the dithering (on is visibly smoother near
> black). Verified on the board: every effect of the tour and of the app runs within its frame
> (times in `strip status`), the `strip` shell commands, settings surviving a reset, the TMP112
> and LIS2DH12 reads. Still to be signed off by eye: the look of the new effects, the thermometer
> bar and the tilt modes. The current figures are the model's estimates, not measurements.

Samples: `strip` (a tour of every effect, under a current limit), `strip_thermometer` (the
temperature as a bar), `strip_tilt` (a spirit level and a rolling ball from the accelerometer).
And an app, `led_strip`: every effect with live control from the shell (see *The `led_strip`
app*).

## Quick start

```rust
use embassy_time::{Instant, Timer};
use tower::strip::{LedKind, Strip};
use tower::{app, board::Board, ws2812};

async fn run(b: Board) {
    let mut strip = Strip::<144, { ws2812::rgbw_buf_len(144) }>::new(
        b.strip_tim, b.strip_data, b.strip_dma, LedKind::Rgbw, 40);
    strip.set_power_limit(1500); // mA, the whole strip
    let start = Instant::now();
    loop {
        strip.rainbow((start.elapsed().as_millis() / 20) as u32);
        strip.show().await;
        Timer::after_millis(20).await;
    }
}

app!(run);
```

`Strip` derefs to the `Frame`, so every effect and composition call works on it directly; `show`
renders the frame over the wire.

## Hardware and the driver

The data line is PA1, driven by **TIM2 channel 2** in PWM mode at 800 kHz: one timer period is one
protocol bit, and **DMA1 channel 3** writes each bit's compare value into `CCR2` on every update
event. A '0' is high for 0.375 µs and a '1' for 0.69 µs (6 and 11 ticks at the 16 MHz sysclk),
the middle of the windows the WS2812B and SK6812 datasheets share. After a frame the line stays
low for 300 µs, the latch the newer WS2812B needs (the SK6812 needs 80 µs).

- **RAM.** The compare buffer holds one byte per bit (the DMA reads bytes and writes 16-bit
  words), so 144 RGBW pixels take 4.6 KB; `ws2812::rgbw_buf_len(n)` sizes it. The frame adds 3
  bytes a pixel.
- **Byte order.** G, R, B (and W) on the wire, the order of the WS2812B and the SK6812. A strip
  that shows the wrong colours in the tour's `channel test` (red, green, blue, white quarters)
  has another order.
- **Cancel safety.** A `show()` dropped mid-frame (for example by a `select` with a timeout)
  stops the DMA and parks the line low. Without that the timer would repeat the last bit forever,
  and an endless run of '1' bits lights every channel of every LED at full (about 10 A on 144 RGBW
  pixels).
- **Power.** The strip needs its own 5 V supply sized for it; at full white 144 RGBW pixels draw
  several amperes, and even dark each LED draws about 1 mA. The Core Module's 3.3 V data line is
  usually read correctly by 5 V strips; a level shifter (the Power Module has one) makes it
  certain.
- **Timing.** A 144-pixel RGBW frame takes about 6 ms on the wire and 5 to 7 ms to render in
  total on the M0+ (gamma, dithering, encoding); the effects add 0.1 to 3 ms. Every effect fits a
  20 ms (50 fps) frame.

## The render pipeline

`show` turns the frame's *intent* colours into wire levels, per pixel:

1. **Orientation.** `set_reversed(true)` sends the frame last pixel first, for a strip mounted
   the other way round. Pixel 0 is the input end otherwise, and moving effects travel away from
   it.
2. **Brightness and gamma.** Each channel times the global brightness (0 to 100 %) goes through
   the gamma curve to linear light. `set_gamma` picks the curve: `Gamma::Linear`, `G2_0` (the
   default), `G2_2` (the sRGB standard) or `G2_8` (steeper, more of the range in the dark end).
   With gamma 2.0 equal steps of intent look like equal steps of brightness, so the brightness
   knob and the fades are perceptual.
3. **White extraction** (RGBW). The white part of each colour, `min(r, g, b)` in linear light,
   moves to the W LED. The colour stays the same (up to the tint of the white LED), white uses
   the real white LED at about a third of the current, and saturated colours are unchanged.
4. **Current limit.** `set_power_limit(ma)` caps every frame: a frame whose estimated current
   (from its light and the `PowerModel`, 20 mA a channel and 1 mA idle per LED by default, on
   the safe side) would exceed the limit is scaled down as a whole, keeping its colour balance.
   `current_ma()` reports the last frame's estimate.
5. **Dithering.** The LEDs take 8-bit levels, and after gamma the dimmest levels are big steps
   to the eye, so a fade near black would visibly step. Each channel keeps a fraction, which
   rounds up or down against a fixed per-pixel threshold spread evenly along the strip: a level
   of 1.5 lights half the pixels at 1 and half at 2, so the strip's brightness moves in steps up to
   256 times finer. The pattern is fixed, so it never flickers; a static colour at a fractional
   level is then not perfectly uniform (neighbours differ by one level), and `set_dither(false)`
   rounds every pixel alike instead.

## Effects

Effects fill the whole frame for a step counter `t`. Derive `t` from the clock
(`elapsed_ms / 20`), not from a frame count, so the speed does not depend on how long a frame takes.

| Effect | What it shows |
|---|---|
| `rainbow(t)` | The colour wheel along the strip, scrolling; constant power (no dark bands at yellow, cyan, magenta) |
| `palette_scroll(&p, t)` | A palette along the strip, scrolling |
| `plasma(&p, t)` | Two sine waves crossing, looked up in a palette: a slow organic flow |
| `twinkle(c, t, density)` | Stars twinkling on their own (stateless, from a hash of the pixel) |
| `candle(t)` | Warm flames flickering from smooth noise, redder when dimmer |
| `scanner(c, t, tail)` | The Larson scanner: a bouncing head with a trail that folds back at the ends |
| `running_lights(c, t)` | Waves of brightness rolling along |
| `theater_chase(c, t)` | Every third pixel, marching |
| `color_wipe(c, t)`, `wipe_in_out(c, t)`, `curtain(c, t)` | Fills: one way, in and out, from the centre |
| `breathe(c, t)` | The whole strip breathing, eased at the top and at black |
| `sunrise(progress)` | Night to daylight through deep red and orange, 16-bit for a 30-minute wake-up light |
| `clock(h, m, s)` | A clock face along the strip: hour red, minute green, second blue, faint hour marks |
| `sparkle(c, &mut rng)`, `meteor(c, t, size, &mut rng)` | Flashes and a meteor whose trails stay in the frame (call every frame without clearing) |
| `Fire::step` + `draw` | A heat simulation rising from pixel 0 (one byte of state a pixel) |
| `Balls::step(dt_ms)` + `draw` | Balls bouncing under gravity, real-time |

Composition: `fill`, `set`, `segments`, `gradient`, `fill_palette`, `bar` (a smooth level bar),
`dot` (anti-aliased, at sub-pixel positions), `fade`, `blur`, `mirror`; `pixels_mut()` for effects
of your own. Colours: the named constants, `hue` (the constant-power wheel), `hsv`, `parse_color`
(`"warm"`, `"ff8000"`), and `Rgb::blend` / `scale` / `add`. Palettes: `palette::LAVA`, `OCEAN`,
`FOREST`, `PARTY` (cyclic), `HEAT`, `SUNRISE` (ramps), or your own 16 colours. Seed the `Rng` with
`board::rand_u32()` for patterns that differ every boot.

## The `led_strip` app

`just flash app led_strip`. The button: a click steps to the next effect, a hold turns the strip
off and on. Everything else is the `strip` shell menu (`tower shell`, the console TUI, or one-shot
`tower exec "strip effect fire"`):

| Command | Effect |
|---|---|
| `strip status` | The effect and every setting, the estimated current, the frame and effect times |
| `strip effect <name>`, `strip next` | `rainbow palette fire twinkle candle plasma scanner balls meteor sparkle running chase wipe curtain breathe solid sunrise clock` |
| `strip on`, `strip off` | Light or blank the strip |
| `strip brightness <0-100>` | Global brightness (perceptual) |
| `strip color <name\|rrggbb>` | The colour of the one-colour effects |
| `strip palette <name>` | The palette of `palette` and `plasma` |
| `strip speed <10-1000>` | Animation speed, % of normal |
| `strip gamma <linear\|2.0\|2.2\|2.8>`, `strip dither <on\|off>`, `strip reverse <on\|off>` | The render settings |
| `strip limit <mA\|off>` | The current cap (1500 mA by default) |
| `strip time <hh:mm[:ss]>` | Set the clock (it counts from 00:00 at boot) |
| `strip sunrise <minutes>` | Start a sunrise of that length |
| `strip save` | Keep the settings in EEPROM; they load at boot |

Its stack peaks at 2.8 KB (measured); the RAM budget gives it its own 6 KB floor
(`tools/check_ram_budget.py`), since the strip's 4.6 KB buffer leaves 7.8 KB of stack.

## Not (yet) here

- **A radio-controlled strip.** A strip node in the radio network (effects set over MQTT through
  the gateway) needs the radio stack's RAM (about 4 KB of statics and a measured 6.8 KB stack
  peak) next to the strip's 4.6 KB buffer, which does not fit the 20 KB part. It needs a streaming
  encoder first: a small circular DMA buffer refilled from the frame on the half-transfer
  interrupt, instead of the whole frame encoded up front.
- **Temporal dithering**, which would also vary the pattern over time: at about 70 to 80 fps it
  could shimmer near black, so only the spatial pattern is used.
