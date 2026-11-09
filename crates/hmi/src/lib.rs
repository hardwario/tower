//! tower-hmi (`crates/hmi`): the hardware-free core of the HARDWARIO TOWER HMI framework.
//!
//! * [`frame`]: a 1-bit framebuffer with per-row dirty tracking, so a driver sends only the rows
//!   that changed (a memory LCD keeps its picture without refresh, so an unchanged screen costs
//!   nothing).
//! * [`canvas`]: drawing on a frame: pixels, lines, rectangles (also rounded), circles, text and
//!   images, with clipping and rotation.
//! * [`font`] and [`fonts`]: monospaced bitmap fonts; the bundled [Spleen](fonts) sizes from
//!   5x8 to 32x64.
//! * [`image`](mod@image): 1-bit images, and [`image!`](macro@image) to draw one as ASCII art at
//!   compile time.
//! * [`menu`]: declarative menus (`static` trees of items) with navigation, value editing and
//!   rendering.
//! * [`input`]: button gestures (click, double click, hold, repeat, chords) from sampled button
//!   states.
//! * [`gesture`]: swipe direction from an APDS-9960 style gesture engine's samples.
//! * [`turn`]: turning the screen: rotation animation steps, and which way up from an
//!   accelerometer.
//!
//! It is not a display driver: the firmware SDK's `tower::lcd` drives the TOWER LCD Module with
//! it. Split out of the `no_std` firmware (which can't `cargo test`) so the logic is unit-tested
//! on the host, like `tower-kv` and `tower-shell`.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod canvas;
pub mod font;
pub mod fonts;
pub mod frame;
pub mod geom;
pub mod gesture;
pub mod image;
pub mod input;
pub mod menu;
pub mod turn;

pub use canvas::{Align, Canvas, Color, Rotation, TextStyle, Window};
pub use font::Font;
pub use frame::Frame;
pub use geom::{Point, Rect};
pub use image::Image;
