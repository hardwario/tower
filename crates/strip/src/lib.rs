//! tower-strip (`crates/strip`): the hardware-free core of the HARDWARIO TOWER addressable LED
//! strip effects.
//!
//! * [`frame`]: [`Frame`], one frame of the strip as RGB *intent* (what the effect means, before
//!   brightness and gamma), with composition (fill, segments, gradient, bar, anti-aliased dots,
//!   fade, blur, mirror) and the animated effects (rainbow, palettes, chase, wipes, breathe,
//!   scanner, twinkle, candle, plasma, sparkle, meteor, sunrise, clock).
//! * [`sim`]: effects with their own state: [`Fire`] (a heat simulation) and [`Balls`]
//!   (bouncing balls under gravity).
//! * [`color`]: [`Rgb`] / [`Rgbw`], named colours, a constant-power [`hue`] wheel, [`hsv`],
//!   blending and parsing.
//! * [`palette`]: 16-colour gradient [`Palette`]s and the built-in set.
//! * [`render`]: [`Render`], the pipeline from intent to the levels on the wire: global
//!   brightness, a selectable [`Gamma`] curve, the RGBW white extraction, a current limiter
//!   and spatial dithering, with the strip's orientation.
//! * [`math`]: the integer sine, noise and hash the effects share; [`rng`]: a small PRNG.
//!
//! Everything is integer math (no float at run time; the tables are computed at compile time)
//! and allocation-free, for a Cortex-M0+ without a divider or an FPU.
//!
//! **Animation time.** Effects take a step counter `t` (`u32`, so it does not wrap in any
//! realistic run) and are pure functions of it, except [`Frame::sparkle`] and
//! [`Frame::meteor`], which keep their trails in the frame. Derive `t` from a clock (for example
//! `elapsed_ms / 20`) rather than counting frames, so an effect's speed does not depend on how
//! long a frame takes to send.
//!
//! **Direction.** Pixel 0 is the input end (where the data line enters). Moving effects travel
//! away from it; [`Render::reversed`] flips the whole strip for one mounted the other way.
//!
//! It is not a driver: the firmware SDK's `tower::strip` drives a WS2812B / SK6812 strip with
//! it (over `tower::ws2812`). Split out of the `no_std` firmware (which can't `cargo test`) so
//! the logic is unit-tested on the host, like `tower-hmi`.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod color;
pub mod frame;
pub mod gamma;
pub mod math;
pub mod palette;
pub mod render;
pub mod rng;
pub mod sim;

pub use color::{
    BLUE, CYAN, GREEN, MAGENTA, OFF, ORANGE, PINK, PURPLE, RED, Rgb, Rgbw, WARM_WHITE, WHITE, YELLOW, hsv,
    hue, parse_color,
};
pub use frame::{Frame, Segment};
pub use gamma::Gamma;
pub use palette::Palette;
pub use render::{LedKind, PowerModel, Render};
pub use rng::Rng;
pub use sim::{Balls, Fire};
