//! Addressable LED **strip**: the [`tower_strip`] effects core on the [`ws2812`](crate::ws2812)
//! driver (WS2812B / SK6812 on PA1).
//!
//! A [`Strip`] owns a [`Frame`] of RGB *intent* plus the driver and the [`Render`] settings.
//! Compose a frame with the effects (it derefs to the [`Frame`], so `strip.rainbow(t)` works),
//! then [`show`](Strip::show) renders it over the wire: brightness, a selectable gamma curve,
//! the RGBW white extraction, an optional current limit and spatial dithering (the pipeline is
//! documented in [`tower_strip::render`]).
//!
//! ```ignore
//! bind_interrupts!(struct Irqs { DMA1_CHANNEL2_3 => ws2812::Refill; });
//!
//! let mut strip = Strip::<144, { ws2812::rgbw_buf_len(144) }>::new(
//!     b.strip_tim, b.strip_data, b.strip_dma, Irqs, LedKind::Rgbw, 50);
//! strip.render.power_limit_ma = 1500; // stay within the supply
//! let start = Instant::now();
//! loop {
//!     strip.rainbow((start.elapsed().as_millis() / 20) as u32); // speed from the clock
//!     strip.show().await;
//!     Timer::after_millis(20).await;
//! }
//! ```
//!
//! Everything in [`tower_strip`] is re-exported here: the effects ([`Frame`]), the simulations
//! ([`Fire`], [`Balls`]), colours ([`hue`], [`hsv`], [`parse_color`], the named colours),
//! [`palette`]s, [`Gamma`], [`Rng`]. Seed the [`Rng`] with
//! [`board::rand_u32`](crate::board::rand_u32) (the TRNG) for patterns that differ every boot.
//!
//! **Timing:** a frame of 144 RGBW pixels takes about 6 ms on the wire (1.25 µs a bit), plus
//! the per-pixel render and the 300 µs latch; the effects themselves are cheap.

use core::cell::Cell;
use core::ops::{Deref, DerefMut};

use embassy_stm32::Peri;
use embassy_stm32::peripherals::{DMA1_CH3, PA1, TIM2};

pub use tower_strip::*;

use embassy_stm32::interrupt::typelevel::{Binding, DMA1_CHANNEL2_3};

use crate::ws2812::{Refill, Ws2812, rgbw_buf_len};

/// A WS2812B / SK6812 strip: a [`Frame`] to draw on, the [`Render`] settings and the driver.
///
/// `NUM` is the pixel count; `BUF` is the driver buffer length: always pass
/// `{ ws2812::rgbw_buf_len(NUM) }` (it fits both RGB and RGBW).
pub struct Strip<'d, const NUM: usize, const BUF: usize> {
    ws: Ws2812<'d, BUF>,
    frame: Frame<NUM>,
    /// How [`show`](Self::show) renders the frame: brightness, gamma, dithering, orientation,
    /// current limit. Change it any time; it applies from the next `show`.
    pub render: Render,
    current_ma: u32,
}

impl<'d, const NUM: usize, const BUF: usize> Strip<'d, NUM, BUF> {
    /// Compile-time guard: `BUF` must hold the full RGBW encoding of `NUM` pixels, so either
    /// wire format fits. A wrong `BUF` (anything but `{ ws2812::rgbw_buf_len(NUM) }`) fails the
    /// **build**, unlike a `debug_assert!`, which would be compiled out in release and only trap
    /// (or silently overflow) on hardware.
    const _BUF_FITS: () = assert!(BUF >= rgbw_buf_len(NUM), "Strip: BUF must be rgbw_buf_len(NUM)");

    /// A strip on PA1 (TIM2_CH2 + DMA1_CH3), all pixels off. `irq` is the app's binding of
    /// `DMA1_CHANNEL2_3` to [`ws2812::Refill`](crate::ws2812::Refill). `brightness` is 0 to
    /// 100 %; the other render settings start at their defaults (gamma 2.0, dithering on, not
    /// reversed, no current limit).
    pub fn new(
        tim: Peri<'d, TIM2>,
        data: Peri<'d, PA1>,
        dma: Peri<'d, DMA1_CH3>,
        irq: impl Binding<DMA1_CHANNEL2_3, Refill> + 'd,
        kind: LedKind,
        brightness: u8,
    ) -> Self {
        let () = Self::_BUF_FITS; // force the compile-time BUF check for this (NUM, BUF)
        Self {
            ws: Ws2812::new(tim, data, dma, irq),
            frame: Frame::new(),
            render: Render::new(kind, brightness),
            current_ma: 0,
        }
    }

    /// Set the global brightness (0 to 100 %, applied before gamma at [`show`](Self::show)).
    pub fn set_brightness(&mut self, percent: u8) {
        self.render.brightness = percent.min(100);
    }

    /// The gamma curve [`show`](Self::show) applies ([`Gamma::G2_0`] by default).
    pub fn set_gamma(&mut self, gamma: Gamma) {
        self.render.gamma = gamma;
    }

    /// Spatial dithering at [`show`](Self::show) (on by default): smooth fades near black, at
    /// the cost of one-level pixel-to-pixel differences in a static colour. Off rounds every pixel
    /// to the nearest level.
    pub fn set_dither(&mut self, on: bool) {
        self.render.dither = on;
    }

    /// The strip is mounted the other way round: pixel 0 of the frame goes to the far end.
    pub fn set_reversed(&mut self, reversed: bool) {
        self.render.reversed = reversed;
    }

    /// Keep every frame within `ma` milliamperes, idle draw included (0 = no limit); see
    /// [`PowerModel`] for the estimate.
    pub fn set_power_limit(&mut self, ma: u32) {
        self.render.power_limit_ma = ma;
    }

    /// The estimated current of the last frame [`show`](Self::show) sent, after the limit, in mA.
    pub fn current_ma(&self) -> u32 {
        self.current_ma
    }

    /// Late ring refills since boot (see [`ws2812`](crate::ws2812), "Underruns"); a frame with
    /// one shows wrong colours from that point. Should stay at 0.
    pub fn underruns(&self) -> u32 {
        crate::ws2812::underruns()
    }

    /// Frames cut short for an EEPROM write since boot (see [`ws2812`](crate::ws2812)).
    pub fn cuts(&self) -> u32 {
        crate::ws2812::cuts()
    }

    /// Render the frame over the wire. Encodes straight from the frame: no second pixel array.
    pub async fn show(&mut self) {
        let fb = self.frame.pixels();
        let factor = self.render.power_factor(fb);
        let levels = Cell::new(0u32);
        let px = self.render.output(fb, factor).inspect(|p| {
            levels.set(levels.get() + p.r as u32 + p.g as u32 + p.b as u32 + p.w as u32);
        });
        match self.render.kind {
            LedKind::Rgb => self.ws.write_rgb_iter(px.map(|p| Rgb::new(p.r, p.g, p.b))).await,
            LedKind::Rgbw => self.ws.write_rgbw_iter(px).await,
        }
        self.current_ma = self.render.levels_ma(levels.get(), NUM);
    }
}

impl<const NUM: usize, const BUF: usize> Deref for Strip<'_, NUM, BUF> {
    type Target = Frame<NUM>;

    fn deref(&self) -> &Frame<NUM> {
        &self.frame
    }
}

impl<const NUM: usize, const BUF: usize> DerefMut for Strip<'_, NUM, BUF> {
    fn deref_mut(&mut self) -> &mut Frame<NUM> {
        &mut self.frame
    }
}
