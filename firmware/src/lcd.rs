//! The TOWER LCD Module: a 128 x 128 Sharp memory LCD, two push buttons, an RGB LED (six LEDs on
//! one set of color lines), a backlight output and an APDS-9960 gesture sensor, as one driver with
//! the [`tower_hmi`] framework on top (drawing, text, images, menus, button and swipe gestures).
//!
//! ```ignore
//! use tower::hmi::{Color, TextStyle, fonts};
//! use tower::{i2c::I2cBus, lcd};
//!
//! let bus = I2cBus::share(b.tmp112.release());
//! let res = lcd::Resources::new(b.header_spi, b.button, b.header_int);
//! let lcd = lcd::init(b.spawner, res, bus, lcd::Config::default()).await.unwrap();
//! lcd.draw(|c| {
//!     c.clear(Color::White);
//!     c.text(0, 0, "Hello", &TextStyle::new(&fonts::SPLEEN_8X16));
//! }).await;
//! loop {
//!     if let lcd::Event::Button(lcd::Button::Right, lcd::Kind::Click) = lcd.next_event().await {
//!         lcd.led().blink(lcd::Rgb::GREEN, 50, 0, 1);
//!     }
//! }
//! ```
//!
//! # Hardware (LCD Module rev 2.4)
//!
//! * **Display**: Sharp LS013B7DH03 on the header SPI (SPI2: SCK PB13, MOSI PB15) at 1 MHz, mode 0.
//!   Its chip select (active high) is `NOR(PB12, expander P7)`: both must be low to select it, so
//!   the display can share the header's CS line with other modules. P7 has a 10 kΩ pull-up, so
//!   holding it low costs 330 µA: the driver asserts it only for the length of a transfer.
//!   `EXTMODE` is tied low, so the panel's VCOM (the polarity inversion that keeps a DC bias off
//!   the liquid crystal) is a bit in each SPI command, toggled in software.
//! * **TCA9534A expander** at I²C `0x3C` (A2 strapped high; the schematic's "0x38" is the family
//!   base): P0 backlight (low = on), P1 right button, P2 `DISP` (high = on), P3 left button,
//!   P4/P5/P6 green/red/blue LED (low = on), P7 display select. Its interrupt pin is not connected.
//! * **Buttons**: each also pulls the header `BOOT` line high through a diode, and that line is the
//!   Core Module button's net on PA8. So a press of any button raises PA8 (an EXTI wake), and the
//!   driver reads the expander only while PA8 is high. PA8 high with neither module button down is
//!   the Core Module's own button ([`Button::Core`]). (Holding a button through a reset starts the
//!   ROM bootloader, as with the Core Module button.)
//! * **APDS-9960** gesture sensor at `0x39`, its interrupt on the header `INT` line (PC13).
//!
//! # Low power
//!
//! Nothing polls while idle: a press wakes the MCU through PA8, a swipe through PC13 (when
//! gestures are on), and a memory LCD keeps its picture with no refresh at all. What remains:
//!
//! * **VCOM**: one 2-byte command every [`Config::vcom_period`] (1 s, Sharp's characterized
//!   operating point; a flush toggles it too). Each costs two expander writes (the select) and
//!   the ATSHA204A's re-sleep, about a millisecond awake.
//! * **Flushes send only rows whose pixels changed**: drawing marks rows dirty only when a byte
//!   changes, and the driver keeps a signature of what each panel row shows, so clearing and
//!   redrawing a whole screen sends just the rows that ended up different. The transfer runs on
//!   DMA with the CPU asleep.
//! * **Expander writes are skipped when nothing changes**; I²C bursts re-sleep the ATSHA204A once,
//!   with an async wait ([`I2cBus::rest`]).
//! * **The gesture sensor is off unless asked for** ([`Lcd::set_gestures`]): on, it draws ~0.3 mA.
//! * **Auto-rotation** ([`autorotate`]) leaves orientation tracking to the accelerometer, which
//!   wakes the MCU only when the device turns.
//!
//! RAM: about 2.9 KB (the 2 KB frame, 512 B of row signatures, a DMA chunk buffer, the tasks).

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, Either3, Either4, select, select3, select4};
use embassy_stm32::exti::ExtiInput;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::mode::Async;
use embassy_stm32::spi::{self, Spi};
use embassy_stm32::time::Hertz;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, ThreadModeRawMutex};
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use tower_hmi::frame::{Frame, row_signature};
use tower_hmi::gesture::{Decoder, Sample};
use tower_hmi::input::{ButtonEvent, Recognizer};
use tower_hmi::menu::{MenuState, MenuStyle, Nav, Outcome, Values};
use tower_hmi::{Canvas, Rotation};

pub use tower_hmi::gesture::Gesture;
pub use tower_hmi::input::Kind;

pub mod auto_rotate;
pub use auto_rotate::{AutoRotate, AutoRotation, autorotate};

use crate::apds9960::{self, Apds9960, GestureConfig};
use crate::board::{HeaderInt, HeaderSpi, HeaderSpiIrqs};
use crate::clock::delay_us;
use crate::i2c::{I2cBus, RawI2c};
use crate::tca9534a::Tca9534a;

/// Display width and height in pixels.
pub const SIZE: i32 = 128;
const STRIDE: usize = 16;
const ROWS: usize = 128;

/// The frame type behind the canvas.
pub type LcdFrame = Frame<STRIDE, ROWS>;

// ---- the expander ------------------------------------------------------------------------------

const EXP_ADDR: u8 = 0x3C;
const P_BACKLIGHT: u8 = 1 << 0; // low = on (P-MOSFET gate)
const P_BUTTON_RIGHT: u8 = 1 << 1; // high = pressed
const P_DISP_ON: u8 = 1 << 2; // high = the panel shows its memory
const P_BUTTON_LEFT: u8 = 1 << 3; // high = pressed
const P_GREEN: u8 = 1 << 4; // low = on
const P_RED: u8 = 1 << 5;
const P_BLUE: u8 = 1 << 6;
const P_SELECT: u8 = 1 << 7; // low (with PB12 low) selects the display; 330 µA while low
const LEDS: u8 = P_GREEN | P_RED | P_BLUE;
/// The configuration register value: the buttons are inputs, every other pin an output.
const INPUTS: u8 = P_BUTTON_RIGHT | P_BUTTON_LEFT;
/// Outputs at rest: backlight, LEDs and select off, display off (until its memory is cleared).
const OUT_IDLE: u8 = P_BACKLIGHT | LEDS | P_SELECT;

/// The expander's output register as last written (changed only under the bus lock).
static OUT: AtomicU8 = AtomicU8::new(OUT_IDLE);

/// Set the `mask` bits of the expander's outputs to `value`, writing only if that changes them.
/// Raw traffic: the caller follows up with [`I2cBus::rest`].
fn set_out(bus: I2cBus, mask: u8, value: u8) -> bool {
    bus.raw(|b| {
        let old = OUT.load(Ordering::Relaxed);
        let new = (old & !mask) | (value & mask);
        if new == old {
            return true;
        }
        let ok = Tca9534a::new(b, EXP_ADDR).write_output(new).is_ok();
        if ok {
            OUT.store(new, Ordering::Relaxed);
        }
        ok
    })
}

/// Board hygiene at boot (from [`Board::take`](crate::board::Board::take), on the raw bus before
/// the ATSHA is put to sleep): an attached LCD Module keeps its state across an MCU reset, so
/// power down a running gesture sensor and release a display select left asserted. Each part is
/// identified first: the APDS-9960 by its ID register, the expander by carrying exactly this
/// driver's pin directions, so another device at either address is never written to.
pub(crate) fn park(i2c: &mut RawI2c) {
    let mut apds = Apds9960::new(&mut *i2c);
    if apds.id().is_ok_and(apds9960::is_id) {
        let _ = apds.power_down();
    }
    let mut exp = Tca9534a::new(&mut *i2c, EXP_ADDR);
    if exp.read_config() == Ok(INPUTS) && exp.read_polarity() == Ok(0) {
        // Display off too: no VCOM toggles are coming to keep a DC bias off the panel.
        let _ = exp.write_output(OUT_IDLE);
    }
}

// ---- the display -------------------------------------------------------------------------------

/// Mode byte bits, sent MSB first: M0 (data update), M1 (VCOM), M2 (all clear).
const M_UPDATE: u8 = 0x80;
const M_VCOM: u8 = 0x40;
const M_CLEAR: u8 = 0x20;
/// One line on the wire: gate address, 16 data bytes, 8 dummy bits.
const LINE: usize = 1 + STRIDE + 1;
/// Lines per DMA transfer.
const CHUNK_LINES: usize = 8;

struct Hw {
    spi: Spi<'static, Async, spi::mode::Master>,
    cs: Output<'static>,
}

/// What both modes share: the VCOM bit and the transfer buffer (a mode byte, eight lines, the
/// trailing dummy byte). All zero bytes initially, so the static is in `.bss`; the hardware
/// handles live apart in [`HW`] (an `Option` of them is not all zeros when `None`, which would
/// move the buffer into `.data`, a flash copy initialized at every boot).
struct Panel {
    vcom: bool,
    buf: [u8; 1 + CHUNK_LINES * LINE + 1],
}

/// Frame mode ([`init`]): the frame, and the signature of what each panel row shows
/// ([`row_signature`] of the frame row sent). Referenced only from the frame-mode code, so the
/// linker leaves these 2.6 KB out of a page-mode app.
struct FrameState {
    frame: LcdFrame,
    sent: [u32; ROWS],
}

type Guard<T> = embassy_sync::mutex::MutexGuard<'static, ThreadModeRawMutex, T>;

static PANEL: Mutex<ThreadModeRawMutex, Panel> = Mutex::new(Panel {
    vcom: false,
    buf: [0; 1 + CHUNK_LINES * LINE + 1],
});
/// The SPI and the chip select; always locked after [`PANEL`] (see [`lock`]).
static HW: Mutex<ThreadModeRawMutex, Option<Hw>> = Mutex::new(None);
/// Frame mode's state; always locked before [`PANEL`].
static FRAME: Mutex<ThreadModeRawMutex, FrameState> = Mutex::new(FrameState {
    frame: Frame::new(),
    sent: [0; ROWS],
});

/// Lock the panel and its hardware, in that order (the only order either is taken in; frame mode
/// takes [`FRAME`] before both).
async fn lock() -> (Guard<Panel>, Guard<Option<Hw>>) {
    let p = PANEL.lock().await;
    (p, HW.lock().await)
}
/// When the panel's VCOM last flipped.
static LAST_VCOM: BlockingMutex<CriticalSectionRawMutex, Cell<Instant>> =
    BlockingMutex::new(Cell::new(Instant::from_ticks(0)));
/// Drawing waits until then: a shell `lcd dump` in progress (see `cmd_dump`).
static DUMP_HOLD: BlockingMutex<CriticalSectionRawMutex, Cell<Instant>> =
    BlockingMutex::new(Cell::new(Instant::from_ticks(0)));
static FLUSHES: AtomicU32 = AtomicU32::new(0);
static ROWS_SENT: AtomicU32 = AtomicU32::new(0);
static VCOM_TOGGLES: AtomicU32 = AtomicU32::new(0);

/// Add to a counter. The M0+ has no atomic read-modify-write; these counters change only in
/// thread mode (the driver's tasks), which never preempts itself, so a load and a store suffice.
fn bump(counter: &AtomicU32, n: u32) {
    counter.store(counter.load(Ordering::Relaxed).wrapping_add(n), Ordering::Relaxed);
}

/// The panel's gate line address of frame row `y` (lines count from 1, sent LSB first).
fn gate(y: usize) -> u8 {
    (y as u8 + 1).reverse_bits()
}

/// The display selected for the life of the value: P7 low over I²C, then PB12 low, then the SCS
/// setup time; dropping it holds SCS, raises PB12, then P7. A drop guard, so a flush whose future
/// is cancelled mid-transfer (an app's `select`) still releases the select, which costs 330 µA
/// while asserted.
struct Selected<'a> {
    bus: I2cBus,
    cs: &'a mut Output<'static>,
}

impl<'a> Selected<'a> {
    fn new(bus: I2cBus, cs: &'a mut Output<'static>) -> Self {
        set_out(bus, P_SELECT, 0);
        cs.set_low();
        delay_us(6); // tsSCS
        Self { bus, cs }
    }
}

impl Drop for Selected<'_> {
    fn drop(&mut self) {
        delay_us(2); // thSCS
        self.cs.set_high();
        set_out(self.bus, P_SELECT, P_SELECT);
    }
}

fn flip_vcom(p: &mut Panel) {
    p.vcom = !p.vcom;
    LAST_VCOM.lock(|c| c.set(Instant::now()));
}

/// The mode byte of a transfer: `mode` with the current VCOM bit.
fn mode_byte(p: &Panel, mode: u8) -> u8 {
    mode | if p.vcom { M_VCOM } else { 0 }
}

/// A command without data (display mode, all clear): the mode byte and 13 or more dummy clocks.
/// Raw I²C traffic: the caller follows with [`I2cBus::rest`], after releasing the display.
fn command(p: &mut Panel, hw: &mut Option<Hw>, bus: I2cBus, mode: u8) {
    if let Some(Hw { spi, cs }) = hw.as_mut() {
        let _selected = Selected::new(bus, cs);
        let _ = spi.blocking_write(&[mode_byte(p, mode), 0]);
        delay_us(25); // SCS high at least 22.55 µs in display mode
    }
    flip_vcom(p);
}

/// Put line `y`'s header and trailer around its pixels at `line` (`LINE` bytes: the gate address,
/// the pixels, the dummy byte), the pixels turned from ink-is-set to the panel's set-is-white.
fn frame_line(line: &mut [u8], y: usize) {
    line[0] = gate(y);
    for b in &mut line[1..1 + STRIDE] {
        *b = !*b;
    }
    line[LINE - 1] = 0;
}

/// Frame mode: send the frame rows that differ from what the panel shows (all of them with
/// `force`). Raw I²C traffic: the caller follows with [`I2cBus::rest`], after releasing the
/// display (the ATSHA's wake delay need not hold up a shell `lcd dump` or another drawer).
///
/// Cancellation-safe: the rows' signatures and dirty marks are updated only once the transfer
/// has completed, so a flush dropped halfway leaves those rows to the next one.
async fn flush(f: &mut FrameState, p: &mut Panel, hw: &mut Option<Hw>, bus: I2cBus, force: bool) {
    let Some(Hw { spi, cs }) = hw.as_mut() else { return };
    let FrameState { frame, sent } = f;
    let mut send = [0u32; ROWS / 32];
    let mut count = 0;
    for y in 0..ROWS {
        if force || (frame.is_dirty(y) && row_signature(frame.row(y)) != sent[y]) {
            send[y / 32] |= 1 << (y % 32);
            count += 1;
        }
    }
    let rows = || (0..ROWS).filter(move |y| send[y / 32] & (1 << (y % 32)) != 0);
    if count > 0 {
        let selected = Selected::new(bus, cs);
        p.buf[0] = mode_byte(p, M_UPDATE);
        let buf = &mut p.buf;
        let mut n = 1;
        for y in rows() {
            if n + LINE > buf.len() - 1 {
                let _ = spi.write(&buf[..n]).await;
                n = 0;
            }
            buf[n + 1..n + 1 + STRIDE].copy_from_slice(frame.row(y));
            frame_line(&mut buf[n..n + LINE], y);
            n += LINE;
        }
        buf[n] = 0; // with the last line's dummy byte, the 16 clocks of the transfer period
        n += 1;
        let _ = spi.write(&buf[..n]).await;
        drop(selected);
        for y in rows() {
            sent[y] = row_signature(frame.row(y));
        }
        bump(&FLUSHES, 1);
        bump(&ROWS_SENT, count);
    }
    frame.clear_all_dirty();
    if count > 0 {
        flip_vcom(p);
    }
}

// ---- page mode ---------------------------------------------------------------------------------

/// Bands of [`CHUNK_LINES`] rows: what page mode renders, compares and sends at a time.
const BANDS: usize = ROWS / CHUNK_LINES;

/// Page mode's renderer ([`init_paged`]).
static RENDER: BlockingMutex<ThreadModeRawMutex, Cell<Option<Renderer>>> =
    BlockingMutex::new(Cell::new(None));
/// The signature of what each band of the panel shows ([`render_band`]).
static BAND_SENT: BlockingMutex<ThreadModeRawMutex, [Cell<u32>; BANDS]> =
    BlockingMutex::new([const { Cell::new(0) }; BANDS]);

/// FNV-1a, continued over `bytes` (a band's signature runs over its rows' pixels).
fn fnv(mut h: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Render band `band` with `render` into `lines` ([`CHUNK_LINES`] lines `stride` bytes apart, the
/// pixels of each at `1..1 + STRIDE`, as in the transfer buffer), ink as a set bit; returns its
/// signature. The rest of each line is left alone.
fn render_band(render: Renderer, lines: &mut [u8], stride: usize, band: usize, rotation: Rotation) -> u32 {
    for l in 0..CHUNK_LINES {
        lines[l * stride + 1..l * stride + 1 + STRIDE].fill(0);
    }
    let window = tower_hmi::Window {
        width: SIZE,
        height: SIZE,
        y0: (band * CHUNK_LINES) as i32,
        rows: CHUNK_LINES as i32,
    };
    let mut dirty = [0u32; 1];
    render(&mut Canvas::window(
        &mut lines[1..],
        &mut dirty,
        stride,
        window,
        rotation,
    ));
    (0..CHUNK_LINES).fold(0x811C_9DC5, |h, l| {
        fnv(h, &lines[l * stride + 1..l * stride + 1 + STRIDE])
    })
}

/// Page mode: render the screen band by band and send the bands whose signature changed (all of
/// them with `force`). Raw I²C traffic: the caller follows with [`I2cBus::rest`].
///
/// A band's signature is recorded once its lines are out, so a show dropped halfway leaves the
/// band it was sending to the next one.
async fn show_bands(p: &mut Panel, hw: &mut Option<Hw>, bus: I2cBus, force: bool) {
    let Some(Hw { spi, cs }) = hw.as_mut() else { return };
    let Some(render) = RENDER.lock(|r| r.get()) else {
        return;
    };
    let rotation = rotation_from(ROTATION.load(Ordering::Relaxed));
    let lines = 1..1 + CHUNK_LINES * LINE;
    // The first band to send, rendered into the buffer: none, nothing to do.
    let next = |buf: &mut [u8], from: usize| {
        (from..BANDS).find_map(|band| {
            let sig = render_band(render, &mut buf[lines.clone()], LINE, band, rotation);
            (force || BAND_SENT.lock(|s| s[band].get()) != sig).then_some((band, sig))
        })
    };
    let Some((mut band, mut sig)) = next(&mut p.buf, 0) else {
        return;
    };
    let selected = Selected::new(bus, cs);
    // The first band sent goes with the mode byte.
    p.buf[0] = mode_byte(p, M_UPDATE);
    let mut from = 0;
    let mut bands = 0;
    loop {
        for l in 0..CHUNK_LINES {
            let at = 1 + l * LINE;
            frame_line(&mut p.buf[at..at + LINE], band * CHUNK_LINES + l);
        }
        let _ = spi.write(&p.buf[from..lines.end]).await;
        BAND_SENT.lock(|s| s[band].set(sig));
        bands += 1;
        from = 1;
        match next(&mut p.buf, band + 1) {
            Some(n) => (band, sig) = n,
            None => break,
        }
    }
    // With the last line's dummy byte, the 16 clocks of the transfer period.
    let _ = spi.blocking_write(&[0u8]);
    drop(selected);
    flip_vcom(p);
    bump(&FLUSHES, 1);
    bump(&ROWS_SENT, (bands * CHUNK_LINES) as u32);
}

// ---- configuration, events, handle -------------------------------------------------------------

/// What [`init`] takes from the [`Board`](crate::board::Board).
pub struct Resources {
    /// The header SPI (`b.header_spi`).
    pub spi: HeaderSpi,
    /// The Core Module button line, PA8 (`b.button`): every module button raises it too.
    pub button: ExtiInput<'static, Async>,
    /// The header `INT` line, PC13 (`b.header_int`): the gesture sensor's interrupt.
    pub int: HeaderInt,
}

impl Resources {
    pub fn new(spi: HeaderSpi, button: ExtiInput<'static, Async>, int: HeaderInt) -> Self {
        Self { spi, button, int }
    }
}

/// Driver settings.
#[derive(Clone, Copy)]
pub struct Config {
    /// How the picture is turned; [`Rotation::R0`] reads with the buttons below the display.
    pub rotation: Rotation,
    /// How often the panel's VCOM is inverted when no flush did it. Sharp characterizes the panel
    /// at 1 Hz; a longer period saves about 3 µA per second of period at the cost of a DC bias on
    /// the liquid crystal (HARDWARIO's earlier C SDK used 15 s).
    pub vcom_period: Duration,
    /// Rewrite the whole panel this often (`None`: never), recovering from a static discharge
    /// that corrupted its memory. About 20 ms of DMA.
    pub refresh_period: Option<Duration>,
    /// Button timing.
    pub buttons: tower_hmi::input::Config,
    /// Button sampling period while a button is down.
    pub scan_interval: Duration,
    /// Start with the gesture sensor on (~0.3 mA; see [`apds9960`]).
    pub gestures: bool,
    /// Gesture sensor settings.
    pub gesture: GestureConfig,
    /// Map the sensor's swipe directions onto the module's (how the sensor is mounted).
    pub gesture_map: fn(Gesture) -> Gesture,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rotation: Rotation::R0,
            vcom_period: Duration::from_secs(1),
            refresh_period: Some(Duration::from_secs(600)),
            buttons: tower_hmi::input::Config::default(),
            scan_interval: Duration::from_millis(10),
            gestures: false,
            gesture: GestureConfig::default(),
            gesture_map: |g| g,
        }
    }
}

/// A button, or both module buttons together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    /// Both module buttons as a chord.
    Both,
    /// The Core Module's own button (PA8 high with neither module button down).
    Core,
}

impl core::fmt::Display for Button {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Both => "both",
            Button::Core => "core",
        })
    }
}

const M_LEFT: u8 = 1;
const M_RIGHT: u8 = 2;
const M_CORE: u8 = 4;

impl Button {
    fn from_mask(m: u8) -> Option<Self> {
        match m {
            M_LEFT => Some(Button::Left),
            M_RIGHT => Some(Button::Right),
            x if x == M_LEFT | M_RIGHT => Some(Button::Both),
            M_CORE => Some(Button::Core),
            _ => None,
        }
    }

    fn mask(self) -> u8 {
        match self {
            Button::Left => M_LEFT,
            Button::Right => M_RIGHT,
            Button::Both => M_LEFT | M_RIGHT,
            Button::Core => M_CORE,
        }
    }
}

/// An input event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Button(Button, Kind),
    Gesture(Gesture),
    /// The screen turned ([`Lcd::rotate`], [`Lcd::rotate_animated`], [`autorotate`]): the picture
    /// turned with it and drawing continues in the new rotation. Nothing needs redrawing, but a
    /// UI whose layout depends on the rotation redraws now.
    Rotated(Rotation),
}

impl core::fmt::Display for Event {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Event::Button(b, k) => write!(f, "{b} {k}"),
            Event::Gesture(g) => write!(f, "swipe {g}"),
            Event::Rotated(r) => write!(f, "rotated to {}°", *r as u16 * 90),
        }
    }
}

/// How [`Lcd::rotate_animated`] animates a turn.
#[derive(Clone, Copy, Debug)]
pub struct Animation {
    /// How long a quarter turn takes (a half turn 1.5 times as long). The panel's own response
    /// (10 to 40 ms) makes much faster pointless.
    pub duration: Duration,
    /// Shrink the picture as it turns, so its corners stay on the screen (back to full size at
    /// the end).
    pub shrink: bool,
}

impl Default for Animation {
    fn default() -> Self {
        Self {
            duration: Duration::from_millis(400),
            shrink: true,
        }
    }
}

/// Why [`Lcd::menu`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuExit {
    /// An action item was entered (the menu state stays where it was: call `menu` again to go on).
    Action(u16),
    /// Back out of the root menu.
    Exit,
    /// No input for the timeout.
    Timeout,
}

/// Counters since boot, for logs and tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Flushes that sent at least one row.
    pub flushes: u32,
    /// Rows sent by them.
    pub rows: u32,
    /// VCOM-only commands (the periodic inversion when no flush did it).
    pub vcom_toggles: u32,
}

/// The driver did not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No expander answered at `0x3C`: no LCD Module attached.
    NotFound,
    /// [`init`] already ran.
    AlreadyInitialized,
}

static INITIALIZED: AtomicBool = AtomicBool::new(false);
/// The picture an animated turn turns ([`Lcd::rotate_animated`]): referenced only from there, so
/// the linker keeps these 2 KB only in apps that animate.
static SNAPSHOT: Mutex<ThreadModeRawMutex, [u8; STRIDE * ROWS]> = Mutex::new([0; STRIDE * ROWS]);
/// The rotation canvases use (a [`Rotation`] as `u8`), applied to the frame by the next update.
static ROTATION: AtomicU8 = AtomicU8::new(Rotation::R0 as u8);

fn rotation_from(r: u8) -> Rotation {
    match r & 3 {
        1 => Rotation::R90,
        2 => Rotation::R180,
        3 => Rotation::R270,
        _ => Rotation::R0,
    }
}
static EVENTS: Channel<CriticalSectionRawMutex, Event, 16> = Channel::new();

enum Cmd {
    Led(LedCmd),
    Backlight(bool),
    DisplayOn(bool),
}

enum Ctrl {
    Gestures(bool),
    Simulate(u8, u32),
    Buttons(tower_hmi::input::Config),
}

static CMDS: Channel<CriticalSectionRawMutex, Cmd, 8> = Channel::new();
static CTRL: Channel<CriticalSectionRawMutex, Ctrl, 4> = Channel::new();

/// Whether an LCD Module is attached (its expander answers): for an app that runs with or
/// without one, before it hands [`init`] the resources.
pub async fn probe(bus: I2cBus) -> bool {
    let found = bus.raw(|b| Tca9534a::new(b, EXP_ADDR).read_config().is_ok());
    bus.rest().await;
    found
}

/// Start the driver in frame mode: set up the expander, clear the panel and turn it on, and spawn
/// its two tasks (VCOM and LED timing; input). The screen starts white with the backlight and LED
/// off. The frame costs 2.6 KB of RAM; [`init_paged`] draws without one.
pub async fn init(spawner: Spawner, res: Resources, bus: I2cBus, cfg: Config) -> Result<Lcd, Error> {
    let spi = start(res.spi, bus, &cfg).await?;
    spawner.spawn(
        input(
            bus,
            res.button,
            res.int,
            InputCfg {
                buttons: cfg.buttons,
                scan: cfg.scan_interval,
                gestures: cfg.gestures,
                gesture: cfg.gesture,
                map: cfg.gesture_map,
            },
        )
        .unwrap(),
    );
    {
        let mut f = FRAME.lock().await;
        let (mut p, mut hw) = lock().await;
        *hw = Some(spi);
        f.frame.set_rotation(cfg.rotation);
        f.sent = [row_signature(&[0; STRIDE]); ROWS];
        power_on(&mut p, &mut hw, bus);
    }
    DUMPER.lock(|d| d.set(Some(dump_frame)));
    set_out(bus, P_DISP_ON, P_DISP_ON);
    bus.raw(|b| {
        let mut s = Apds9960::new(b);
        if cfg.gestures {
            s.start_gestures(&cfg.gesture)
        } else {
            s.power_down()
        }
    })
    .ok();
    bus.rest().await;
    spawner.spawn(service(bus, cfg.vcom_period, cfg.refresh_period).unwrap());
    Ok(Lcd::new(bus))
}

/// What draws a page-mode screen ([`init_paged`]): the whole screen, every time, from the app's
/// state (a `static` the app updates before [`Lcd::show`]). It runs once per band of rows, with
/// drawing clipped to the band; it must not block, and it draws the same screen each time until
/// the app changes its state.
pub type Renderer = fn(&mut Canvas<'_>);

/// Start the driver in **page mode**: no frame in RAM. The screen is whatever `render` draws;
/// [`Lcd::show`] runs it once per band of 8 rows into the driver's 146 B transfer buffer and sends
/// the bands that changed (a signature per band). That saves 2.5 KB of RAM over [`init`] (the
/// frame and the row signatures) at the cost of drawing the whole screen per band: about the
/// time a frame-mode full redraw takes, sixteen times over (milliseconds per band for text and
/// fills). A radio node's screen fits this way.
///
/// Lean on purpose, for a node short of RAM and flash: the buttons only (no swipes; the gesture
/// sensor stays powered down and its decoding out of the image, [`Config::gestures`] is
/// ignored), and the periodic refresh rides on the app's next [`show`](Lcd::<Paged>::show) (an
/// app showing a clock shows every minute anyway). `lcd dump` runs `render`. Otherwise as
/// [`init`]; `res.int` is not used.
pub async fn init_paged(
    spawner: Spawner,
    res: Resources,
    bus: I2cBus,
    cfg: Config,
    render: Renderer,
) -> Result<Lcd<Paged>, Error> {
    let spi = start(res.spi, bus, &cfg).await?;
    spawner.spawn(input_keys(bus, res.button, cfg.buttons, cfg.scan_interval).unwrap());
    {
        let (mut p, mut hw) = lock().await;
        *hw = Some(spi);
        let blank = (0..CHUNK_LINES).fold(0x811C_9DC5, |h, _| fnv(h, &[0; STRIDE]));
        BAND_SENT.lock(|s| s.iter().for_each(|b| b.set(blank)));
        power_on(&mut p, &mut hw, bus);
    }
    RENDER.lock(|r| r.set(Some(render)));
    DUMPER.lock(|d| d.set(Some(dump_paged)));
    set_out(bus, P_DISP_ON, P_DISP_ON);
    bus.raw(|b| Apds9960::new(b).power_down()).ok();
    bus.rest().await;
    spawner.spawn(service_paged(bus, cfg.vcom_period, cfg.refresh_period).unwrap());
    Ok(Lcd::new(bus))
}

/// The common start of both modes: the expander (or [`Error::NotFound`]), the SPI. The panel's
/// power-on sequence follows: clear the pixel memory ([`power_on`]), then `DISP` high (30 µs
/// later at least; the expander write alone takes longer).
async fn start(spi: HeaderSpi, bus: I2cBus, cfg: &Config) -> Result<Hw, Error> {
    // Thread mode only (no atomic swap on the M0+), so a load and a store suffice.
    if INITIALIZED.load(Ordering::Relaxed) {
        return Err(Error::AlreadyInitialized);
    }
    INITIALIZED.store(true, Ordering::Relaxed);
    // Outputs first, then directions, so no pin drives a wrong level on the way.
    let found = bus.raw(|b| {
        let mut exp = Tca9534a::new(b, EXP_ADDR);
        exp.write_output(OUT_IDLE).is_ok() && exp.write_config(INPUTS).is_ok()
    });
    if !found {
        bus.rest().await;
        return Err(Error::NotFound);
    }
    OUT.store(OUT_IDLE, Ordering::Relaxed);

    let mut sc = spi::Config::default();
    sc.frequency = Hertz(1_000_000); // the panel's maximum is 1.1 MHz
    sc.mode = spi::MODE_0;
    sc.bit_order = spi::BitOrder::MsbFirst;
    let HeaderSpi {
        spi: spi2,
        sck,
        mosi,
        cs,
        tx_dma,
        ..
    } = spi;
    let spi = Spi::new_txonly(spi2, sck, mosi, tx_dma, HeaderSpiIrqs, sc);
    // SCK idles low on embassy's pull-down while the SPI is disabled between transfers; MOSI
    // would float (an input buffer leaking in STOP), so give it a pull-down too.
    embassy_stm32::pac::GPIOB
        .pupdr()
        .modify(|w| w.set_pupdr(15, embassy_stm32::pac::gpio::vals::Pupdr::PULL_DOWN));
    let cs = Output::new(cs, Level::High, Speed::Low);
    ROTATION.store(cfg.rotation as u8, Ordering::Relaxed);
    Ok(Hw { spi, cs })
}

/// The panel's power-on sequence, first half: clear the pixel memory.
fn power_on(p: &mut Panel, hw: &mut Option<Hw>, bus: I2cBus) {
    command(p, hw, bus, M_CLEAR);
}

/// [`Lcd`]'s mode: [`Framed`] ([`init`]) or [`Paged`] ([`init_paged`]).
pub trait Mode: Clone + Copy {}

/// Frame mode: draw into a frame kept in RAM ([`Lcd::draw`]).
#[derive(Clone, Copy)]
pub struct Framed;

/// Page mode: a renderer draws the screen band by band ([`Lcd::show`]).
#[derive(Clone, Copy)]
pub struct Paged;

impl Mode for Framed {}
impl Mode for Paged {}

/// The handle to the started driver: cheap, `Copy`, usable from any task. Its mode says how the
/// screen is drawn: [`Lcd`] (frame mode, [`init`]) draws with [`draw`](Self::draw), `Lcd<Paged>`
/// ([`init_paged`]) with [`show`](Lcd::<Paged>::show); input, the LED and the rest are the same.
#[derive(Clone, Copy)]
pub struct Lcd<M: Mode = Framed> {
    bus: I2cBus,
    mode: core::marker::PhantomData<M>,
}

impl Lcd<Paged> {
    /// Draw the screen with the renderer and send the bands that changed (every band once the
    /// periodic refresh is due).
    pub async fn show(&self) {
        let force = REFRESH_DUE.load(Ordering::Relaxed);
        self.send(force).await;
    }

    /// Draw the screen and rewrite every band of the panel.
    pub async fn refresh(&self) {
        self.send(true).await;
    }

    #[inline(never)]
    async fn send(&self, force: bool) {
        {
            let (mut p, mut hw) = lock().await;
            show_bands(&mut p, &mut hw, self.bus, force).await;
            if force {
                REFRESH_DUE.store(false, Ordering::Relaxed);
            }
        }
        self.bus.rest().await;
    }
}

impl Lcd {
    /// Draw with `f`, then send what changed to the panel.
    pub async fn draw<R>(&self, f: impl FnOnce(&mut Canvas<'_>) -> R) -> R {
        // Only the drawing is generic; the flush is one function for every call site.
        let r = self.update(f).await;
        self.flush().await;
        r
    }

    /// Draw with `f` without sending anything yet (several updates, one [`flush`](Self::flush)).
    pub async fn update<R>(&self, f: impl FnOnce(&mut Canvas<'_>) -> R) -> R {
        // A shell `lcd dump` pages the frame out over several requests: hold drawing meanwhile, so
        // they all see one frame.
        while Instant::now() < DUMP_HOLD.lock(|c| c.get()) {
            Timer::after_millis(20).await;
        }
        let mut state = FRAME.lock().await;
        let r = ROTATION.load(Ordering::Relaxed);
        if r != state.frame.rotation() as u8 {
            state.frame.set_rotation(rotation_from(r));
        }
        f(&mut state.frame.canvas())
    }

    /// Send what changed since the last flush.
    #[inline(never)]
    pub async fn flush(&self) {
        {
            let mut f = FRAME.lock().await;
            let (mut p, mut hw) = lock().await;
            flush(&mut f, &mut p, &mut hw, self.bus, false).await;
        }
        self.bus.rest().await;
    }

    /// Rewrite every row of the panel.
    pub async fn refresh(&self) {
        {
            let mut f = FRAME.lock().await;
            let (mut p, mut hw) = lock().await;
            flush(&mut f, &mut p, &mut hw, self.bus, true).await;
        }
        self.bus.rest().await;
    }

    /// Turn the screen to `rotation` at once, the picture on it included (in place, exact, no extra
    /// RAM), and send it; drawing continues in the new rotation. Emits [`Event::Rotated`].
    pub async fn rotate(&self, rotation: Rotation) {
        let turned = {
            let mut f = FRAME.lock().await;
            let (mut p, mut hw) = lock().await;
            let q = tower_hmi::turn::quarters(f.frame.rotation(), rotation);
            if q != 0 {
                f.frame.turn_quarters(q);
                f.frame.set_rotation(rotation);
                flush(&mut f, &mut p, &mut hw, self.bus, false).await;
            }
            ROTATION.store(rotation as u8, Ordering::Relaxed);
            q != 0
        };
        self.after_turn(turned, rotation).await;
    }

    /// [`rotate`](Self::rotate), animated: the picture turns on the screen, eased in and out, in
    /// as many steps as `animation.duration` allows (a step is a full-screen redraw and flush,
    /// about 45 ms), and ends exactly on the new rotation. Holds the display meanwhile. Needs a 2 KB
    /// snapshot of the picture (a `static`, linked only into an app that animates).
    pub async fn rotate_animated(&self, rotation: Rotation, animation: Animation) {
        use tower_hmi::turn::{self, ONE, QUARTER};
        let turned = {
            let mut f = FRAME.lock().await;
            let (mut p, mut hw) = lock().await;
            let q = turn::quarters(f.frame.rotation(), rotation);
            if q != 0 {
                let mut snapshot = SNAPSHOT.lock().await;
                snapshot.copy_from_slice(f.frame.bytes());
                let total = animation.duration * if q == 2 { 3 } else { 2 } / 2;
                let start = Instant::now();
                let mut steps = 0u32;
                loop {
                    let t = (start.elapsed().as_millis() * ONE as u64 / total.as_millis().max(1)) as i32;
                    if t >= ONE {
                        break;
                    }
                    let a = q * QUARTER * turn::ease(t) / ONE;
                    let scale = if animation.shrink { turn::fit(a) } else { ONE };
                    f.frame.draw_turned(&snapshot[..], a, scale);
                    flush(&mut f, &mut p, &mut hw, self.bus, false).await;
                    steps += 1;
                }
                // The last step is the exact quarter turn: what drawing in `rotation` gives.
                f.frame.draw_turned(&snapshot[..], q * QUARTER, ONE);
                f.frame.set_rotation(rotation);
                flush(&mut f, &mut p, &mut hw, self.bus, false).await;
                log::debug!(
                    target: "lcd",
                    "turned {}° in {} ms, {} steps",
                    q * 90,
                    start.elapsed().as_millis(),
                    steps + 1
                );
            }
            ROTATION.store(rotation as u8, Ordering::Relaxed);
            q != 0
        };
        self.after_turn(turned, rotation).await;
    }

    async fn after_turn(&self, turned: bool, rotation: Rotation) {
        self.bus.rest().await;
        if turned {
            let _ = EVENTS.try_send(Event::Rotated(rotation));
        }
    }
}

impl<M: Mode> Lcd<M> {
    fn new(bus: I2cBus) -> Self {
        Self {
            bus,
            mode: core::marker::PhantomData,
        }
    }

    /// Change the rotation from the next drawing on ([`draw`](Lcd::draw), [`update`](Lcd::update),
    /// [`show`](Lcd::<Paged>::show); fire-and-forget, so a menu's [`Values::set`] can call it). In
    /// frame mode what is already drawn stays where it is: redraw the whole screen after this
    /// ([`rotate`](Lcd::rotate) turns the picture along).
    pub fn set_rotation(&self, rotation: Rotation) {
        ROTATION.store(rotation as u8, Ordering::Relaxed);
    }

    pub fn rotation(&self) -> Rotation {
        rotation_from(ROTATION.load(Ordering::Relaxed))
    }

    /// The next input event (button gestures, swipes).
    pub async fn next_event(&self) -> Event {
        EVENTS.receive().await
    }

    /// The next input event, if one is waiting.
    pub fn try_event(&self) -> Option<Event> {
        EVENTS.try_receive().ok()
    }

    /// Drop the waiting input events.
    pub fn clear_events(&self) {
        EVENTS.clear();
    }

    /// The RGB LED.
    pub fn led(&self) -> RgbLed {
        RgbLed(())
    }

    /// Switch the backlight output (`BL+`/`BL-` pads, 150 Ω from 3 V; fire-and-forget).
    pub fn set_backlight(&self, on: bool) {
        let _ = CMDS.try_send(Cmd::Backlight(on));
    }

    /// Show the panel's memory (`true`), or white with the memory kept (`false`; fire-and-forget).
    pub fn set_display(&self, on: bool) {
        let _ = CMDS.try_send(Cmd::DisplayOn(on));
    }

    /// Change the button timing (fire-and-forget; applied once no button is down): a game turning
    /// on auto-repeat, say, which a menu would rather not have.
    pub fn set_buttons(&self, config: tower_hmi::input::Config) {
        let _ = CTRL.try_send(Ctrl::Buttons(config));
    }

    /// Inject a press of `button` held for `ms` milliseconds: it goes through the same
    /// recognition as a real one (a test hook, a shell command).
    pub fn simulate(&self, button: Button, ms: u32) {
        simulate(button, ms);
    }

    pub fn stats(&self) -> Stats {
        stats()
    }
}

impl Lcd {
    /// Turn the gesture sensor on or off (~0.3 mA when on; fire-and-forget). Frame mode only: page
    /// mode has no swipes.
    pub fn set_gestures(&self, on: bool) {
        let _ = CTRL.try_send(Ctrl::Gestures(on));
    }

    /// Run `state` as a full-screen menu until an action, a back out of the root, or `timeout`
    /// without input. Values change through `values` as the user edits them. Input is mapped by
    /// [`default_nav`].
    pub async fn menu(
        &self,
        state: &mut MenuState,
        values: &mut impl Values,
        style: &MenuStyle,
        timeout: Option<Duration>,
    ) -> MenuExit {
        self.menu_with(state, values, style, timeout, default_nav).await
    }

    /// [`menu`](Self::menu) with your own mapping of events to navigation.
    pub async fn menu_with(
        &self,
        state: &mut MenuState,
        values: &mut impl Values,
        style: &MenuStyle,
        timeout: Option<Duration>,
        nav: fn(Event) -> Option<Nav>,
    ) -> MenuExit {
        loop {
            self.draw(|c| state.render(c, &*values, style)).await;
            let ev = match timeout {
                Some(t) => match with_timeout(t, self.next_event()).await {
                    Ok(e) => e,
                    Err(_) => return MenuExit::Timeout,
                },
                None => self.next_event().await,
            };
            match nav(ev).and_then(|n| state.handle(n, values)) {
                Some(Outcome::Action(id)) => return MenuExit::Action(id),
                Some(Outcome::Exit) => return MenuExit::Exit,
                _ => {}
            }
        }
    }
}

/// The default menu navigation for two buttons: left click = previous, right click = next, right
/// hold (or both clicked together, or the Core Module button) = enter, left hold = back; swipes up
/// and down move, a swipe right enters and a swipe left goes back.
pub fn default_nav(ev: Event) -> Option<Nav> {
    Some(match ev {
        Event::Button(Button::Left, Kind::Click | Kind::Repeat) => Nav::Prev,
        Event::Button(Button::Right, Kind::Click | Kind::Repeat) => Nav::Next,
        Event::Button(Button::Right | Button::Both, Kind::Hold) => Nav::Enter,
        Event::Button(Button::Both | Button::Core, Kind::Click) => Nav::Enter,
        Event::Button(Button::Left, Kind::Hold) => Nav::Back,
        Event::Gesture(Gesture::Up) => Nav::Prev,
        Event::Gesture(Gesture::Down) => Nav::Next,
        Event::Gesture(Gesture::Right) => Nav::Enter,
        Event::Gesture(Gesture::Left) => Nav::Back,
        _ => return None,
    })
}

// ---- the RGB LED -------------------------------------------------------------------------------

/// An RGB LED color: each channel on or off (the module switches the six LEDs' color lines, no
/// dimming).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub red: bool,
    pub green: bool,
    pub blue: bool,
}

impl Rgb {
    pub const OFF: Rgb = Rgb::new(false, false, false);
    pub const RED: Rgb = Rgb::new(true, false, false);
    pub const GREEN: Rgb = Rgb::new(false, true, false);
    pub const BLUE: Rgb = Rgb::new(false, false, true);
    pub const YELLOW: Rgb = Rgb::new(true, true, false);
    pub const CYAN: Rgb = Rgb::new(false, true, true);
    pub const MAGENTA: Rgb = Rgb::new(true, false, true);
    pub const WHITE: Rgb = Rgb::new(true, true, true);

    pub const fn new(red: bool, green: bool, blue: bool) -> Self {
        Self { red, green, blue }
    }

    /// The expander output bits (active low).
    fn bits(self) -> u8 {
        let on = (self.red as u8 * P_RED) | (self.green as u8 * P_GREEN) | (self.blue as u8 * P_BLUE);
        LEDS & !on
    }
}

/// One step of an LED pattern: a color for `ms` milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct Step {
    pub color: Rgb,
    pub ms: u32,
}

impl Step {
    pub const fn new(color: Rgb, ms: u32) -> Self {
        Self { color, ms }
    }
}

/// An LED pattern: a looping background, or a one-shot sequence.
pub type Pattern = &'static [Step];

/// The module's RGB LED, as [`led::Led`](crate::led::Led) does a plain one: a steady color or a
/// looping background pattern, and one-shot sequences that preempt it and then let it resume.
/// Every method is fire-and-forget.
#[derive(Clone, Copy)]
pub struct RgbLed(());

impl RgbLed {
    /// Show `color` steadily (replaces a background pattern).
    pub fn set(&self, color: Rgb) {
        let _ = CMDS.try_send(Cmd::Led(LedCmd::Set(color)));
    }

    /// Loop `pattern` in the background (`None`: back to the steady color).
    pub fn set_background(&self, pattern: Option<Pattern>) {
        let _ = CMDS.try_send(Cmd::Led(LedCmd::Background(pattern)));
    }

    /// Play `pattern` once, then resume.
    pub fn play(&self, pattern: Pattern) {
        let _ = CMDS.try_send(Cmd::Led(LedCmd::Play(pattern)));
    }

    /// Blink `color` `count` times (`on_ms` on, `off_ms` off), then resume.
    pub fn blink(&self, color: Rgb, on_ms: u32, off_ms: u32, count: u16) {
        let _ = CMDS.try_send(Cmd::Led(LedCmd::Blink {
            color,
            on_ms,
            off_ms,
            count,
        }));
    }
}

enum LedCmd {
    Set(Rgb),
    Background(Option<Pattern>),
    Play(Pattern),
    Blink {
        color: Rgb,
        on_ms: u32,
        off_ms: u32,
        count: u16,
    },
}

/// What the LED is playing.
enum LedSource {
    Steady,
    Background(usize),
    Play(Pattern, usize),
    Blink {
        color: Rgb,
        on_ms: u32,
        off_ms: u32,
        left: u16,
        lit: bool,
    },
}

struct LedMachine {
    steady: Rgb,
    background: Option<Pattern>,
    src: LedSource,
    until: Option<Instant>,
}

impl LedMachine {
    fn new() -> Self {
        Self {
            steady: Rgb::OFF,
            background: None,
            src: LedSource::Steady,
            until: None,
        }
    }

    fn color(&self) -> Rgb {
        match self.src {
            LedSource::Steady => self.steady,
            LedSource::Background(i) => self.background.map_or(self.steady, |p| p[i].color),
            LedSource::Play(p, i) => p[i].color,
            LedSource::Blink { color, lit, .. } => {
                if lit {
                    color
                } else {
                    Rgb::OFF
                }
            }
        }
    }

    /// Enter `src`, starting its step timer.
    fn enter(&mut self, src: LedSource, now: Instant) {
        let ms = match src {
            LedSource::Steady => None,
            LedSource::Background(i) => self.background.map(|p| p[i].ms),
            LedSource::Play(p, i) => Some(p[i].ms),
            LedSource::Blink {
                on_ms, off_ms, lit, ..
            } => Some(if lit { on_ms } else { off_ms }),
        };
        self.src = src;
        self.until = ms.map(|ms| now + Duration::from_millis(ms as u64));
    }

    /// What plays when a one-shot ends.
    fn resume(&mut self, now: Instant) {
        match self.background {
            Some(p) if !p.is_empty() => self.enter(LedSource::Background(0), now),
            _ => self.enter(LedSource::Steady, now),
        }
    }

    fn command(&mut self, cmd: LedCmd, now: Instant) {
        match cmd {
            LedCmd::Set(c) => {
                self.steady = c;
                self.background = None;
                self.enter(LedSource::Steady, now);
            }
            LedCmd::Background(p) => {
                self.background = p.filter(|p| !p.is_empty());
                if matches!(self.src, LedSource::Steady | LedSource::Background(_)) {
                    self.resume(now);
                }
            }
            LedCmd::Play(p) if !p.is_empty() => self.enter(LedSource::Play(p, 0), now),
            LedCmd::Blink {
                color,
                on_ms,
                off_ms,
                count,
            } if count > 0 => self.enter(
                LedSource::Blink {
                    color,
                    on_ms,
                    off_ms,
                    left: count,
                    lit: true,
                },
                now,
            ),
            _ => {}
        }
    }

    /// The current step's time is up.
    fn advance(&mut self, now: Instant) {
        match self.src {
            LedSource::Steady => self.until = None,
            LedSource::Background(i) => {
                let len = self.background.map_or(1, |p| p.len());
                self.enter(LedSource::Background((i + 1) % len), now);
            }
            LedSource::Play(p, i) if i + 1 < p.len() => self.enter(LedSource::Play(p, i + 1), now),
            LedSource::Blink {
                color,
                on_ms,
                off_ms,
                left,
                lit: true,
            } if off_ms > 0 || left > 1 => self.enter(
                LedSource::Blink {
                    color,
                    on_ms,
                    off_ms,
                    left,
                    lit: false,
                },
                now,
            ),
            LedSource::Blink {
                color,
                on_ms,
                off_ms,
                left,
                lit: false,
            } if left > 1 => self.enter(
                LedSource::Blink {
                    color,
                    on_ms,
                    off_ms,
                    left: left - 1,
                    lit: true,
                },
                now,
            ),
            _ => self.resume(now),
        }
    }
}

// ---- tasks -------------------------------------------------------------------------------------

/// Page mode's periodic refresh is due: the next [`Lcd::show`] rewrites every band.
static REFRESH_DUE: AtomicBool = AtomicBool::new(false);

/// How a mode rewrites the whole panel (the periodic refresh).
trait Rewrite {
    async fn rewrite(bus: I2cBus);
}

impl Rewrite for Framed {
    async fn rewrite(bus: I2cBus) {
        let mut f = FRAME.lock().await;
        let (mut p, mut hw) = lock().await;
        flush(&mut f, &mut p, &mut hw, bus, true).await;
    }
}

impl Rewrite for Paged {
    /// Left to the app's next show: the driver has no picture of its own to send, and running the
    /// renderer from here would keep a second copy of the band loop's state in this task.
    async fn rewrite(_bus: I2cBus) {
        REFRESH_DUE.store(true, Ordering::Relaxed);
    }
}

/// The service task of frame mode.
#[embassy_executor::task]
async fn service(bus: I2cBus, vcom_period: Duration, refresh: Option<Duration>) {
    serve::<Framed>(bus, vcom_period, refresh).await
}

/// The service task of page mode.
#[embassy_executor::task]
async fn service_paged(bus: I2cBus, vcom_period: Duration, refresh: Option<Duration>) {
    serve::<Paged>(bus, vcom_period, refresh).await
}

/// VCOM inversion, the periodic full rewrite, the LED's timing and the expander commands. Generic
/// over the mode only for the rewrite, so the other mode's state stays out of the image.
async fn serve<M: Rewrite>(bus: I2cBus, vcom_period: Duration, refresh: Option<Duration>) {
    let mut led = LedMachine::new();
    let mut last_refresh = Instant::now();
    loop {
        let vcom_at = LAST_VCOM.lock(|c| c.get()) + vcom_period;
        let refresh_at = refresh.map(|p| last_refresh + p);
        let wake = [Some(vcom_at), refresh_at, led.until]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(vcom_at);
        match select(CMDS.receive(), Timer::at(wake)).await {
            Either::First(cmd) => {
                match cmd {
                    Cmd::Led(c) => {
                        led.command(c, Instant::now());
                        set_out(bus, LEDS, led.color().bits());
                    }
                    Cmd::Backlight(on) => {
                        set_out(bus, P_BACKLIGHT, if on { 0 } else { P_BACKLIGHT });
                    }
                    Cmd::DisplayOn(on) => {
                        set_out(bus, P_DISP_ON, if on { P_DISP_ON } else { 0 });
                    }
                }
                bus.rest().await;
            }
            Either::Second(()) => {
                let now = Instant::now();
                if led.until.is_some_and(|t| t <= now) {
                    led.advance(now);
                    set_out(bus, LEDS, led.color().bits());
                    bus.rest().await;
                }
                if refresh_at.is_some_and(|t| t <= now) {
                    last_refresh = now;
                    M::rewrite(bus).await;
                } else if vcom_at <= now {
                    let (mut p, mut hw) = lock().await;
                    // A flush may have inverted it meanwhile.
                    if LAST_VCOM.lock(|c| c.get()) + vcom_period <= Instant::now() {
                        command(&mut p, &mut hw, bus, 0);
                        bump(&VCOM_TOGGLES, 1);
                    }
                }
                bus.rest().await;
            }
        }
    }
}

struct InputCfg {
    buttons: tower_hmi::input::Config,
    scan: Duration,
    gestures: bool,
    gesture: GestureConfig,
    map: fn(Gesture) -> Gesture,
}

/// While a button is held past its Hold (nothing else can happen until a release), sample this
/// often instead of every scan interval, besides waking on the all-released edge of PA8.
const HELD_POLL: Duration = Duration::from_millis(250);
/// The longest a single swipe is read for (a hand resting over the sensor keeps it in gesture
/// mode: the reading then starts over).
const GESTURE_MAX: Duration = Duration::from_secs(3);
/// How often the gesture FIFO is read while a swipe is in progress (it holds 32 datasets, about
/// 90 ms of them).
const GESTURE_POLL: Duration = Duration::from_millis(30);
/// Swipes are ignored this long after a button was down: a hand reaching for a button passes
/// over the gesture sensor too.
const SWIPE_QUIET: Duration = Duration::from_millis(400);

/// A swipe being read.
struct Swipe {
    start: Instant,
    next_read: Instant,
    /// A button was down meanwhile: what the sensor saw was the hand at the buttons.
    buttons: bool,
}

/// One activation of the gesture sensor as the driver read it, decoded or not
/// ([`swipe_trace`]): a tuning and test aid.
#[derive(Clone, Copy, Debug)]
pub struct SwipeTrace {
    /// How long the sensor stayed in gesture mode, milliseconds.
    pub ms: u32,
    /// What the decoder made of the datasets read: how many, the brightest, the lags it decides
    /// on.
    pub trace: tower_hmi::gesture::Trace,
    /// The swipe decoded (before [`Config::gesture_map`]), if any.
    pub gesture: Option<Gesture>,
    /// Decoded but dropped: a button was in use.
    pub ignored: bool,
}

static SWIPE_TRACE: Signal<CriticalSectionRawMutex, SwipeTrace> = Signal::new();

/// The next activation of the gesture sensor, every one, decoded or not (the latest if several
/// came since the last call). A tuning aid; an app reacts to [`Event::Gesture`].
pub async fn swipe_trace() -> SwipeTrace {
    SWIPE_TRACE.wait().await
}

/// The buttons' side of the input: the recognizer, an injected press, new timing waiting for the
/// buttons to come up. Shared by both input tasks.
struct Keys {
    rec: Recognizer,
    scan: Duration,
    sim: Option<(u8, Instant)>,
    // New button timing waits until no button is down: a recognizer replaced mid-press would take
    // the buttons still held for a fresh press, and their release for a click (a game quit by
    // holding both buttons would click its way straight back in from the menu).
    new_timing: Option<tower_hmi::input::Config>,
}

impl Keys {
    fn new(buttons: tower_hmi::input::Config, scan: Duration) -> Self {
        Self {
            rec: Recognizer::new(buttons),
            scan,
            sim: None,
            new_timing: None,
        }
    }

    /// Sampling on a timer: a button down (before its Hold), or an injected press.
    fn polling(&self) -> bool {
        self.sim.is_some() || (self.rec.needs_polling() && !self.rec.waiting_for_release())
    }

    /// When to look again without an edge: the recognizer's next deadline, the scan interval
    /// while a button is down, the end of an injected press.
    fn next_at(&self) -> Option<Instant> {
        let mut at = self.rec.next_deadline().map(Instant::from_millis);
        let mut sooner = |t: Instant| at = Some(at.map_or(t, |a| a.min(t)));
        if self.polling() {
            sooner(Instant::now() + self.scan);
        }
        if let Some((_, end)) = self.sim {
            sooner(end);
        }
        at
    }

    /// The PA8 edge worth waiting for: none while polling, the release (or a slow poll) while a
    /// button is held past its Hold, else a press.
    async fn edge(&self, button: &mut ExtiInput<'static, Async>) {
        if self.polling() {
            core::future::pending::<()>().await
        } else if self.rec.waiting_for_release() {
            let _ = with_timeout(HELD_POLL, button.wait_for_low()).await;
        } else {
            button.wait_for_high().await
        }
    }

    fn control(&mut self, c: Ctrl) {
        match c {
            Ctrl::Buttons(c) => self.new_timing = Some(c),
            Ctrl::Simulate(mask, ms) => {
                self.sim = Some((mask, Instant::now() + Duration::from_millis(ms as u64)));
            }
            Ctrl::Gestures(_) => {}
        }
    }

    /// Sample the buttons (the expander, while PA8 is high) and run the recognizer; the buttons
    /// down, and whether the bus was used.
    fn sample(&mut self, now: Instant, bus: I2cBus, button: &ExtiInput<'static, Async>) -> (u8, bool) {
        if self.sim.is_some_and(|(_, end)| now >= end) {
            self.sim = None;
        }
        let mut mask = self.sim.map_or(0, |(m, _)| m);
        let traffic = button.is_high();
        if traffic {
            let pins = bus.raw(|b| Tca9534a::new(b, EXP_ADDR).read_input()).unwrap_or(0);
            let module = (if pins & P_BUTTON_LEFT != 0 { M_LEFT } else { 0 })
                | (if pins & P_BUTTON_RIGHT != 0 { M_RIGHT } else { 0 });
            mask |= if module != 0 { module } else { M_CORE };
        }
        self.rec.update(now.as_millis(), mask, &mut |e: ButtonEvent| {
            if let Some(b) = Button::from_mask(e.buttons) {
                let _ = EVENTS.try_send(Event::Button(b, e.kind));
            }
        });
        if !self.rec.needs_polling()
            && let Some(c) = self.new_timing.take()
        {
            self.rec = Recognizer::new(c);
        }
        (mask, traffic)
    }
}

/// Wait until `at` (forever when `None`).
async fn until(at: Option<Instant>) {
    match at {
        Some(t) => Timer::at(t).await,
        None => core::future::pending().await,
    }
}

/// Frame mode's input: buttons (sampled while PA8 is high) and swipes (read while the gesture
/// sensor reports one), interleaved in one loop so neither waits for the other: a swipe takes a
/// FIFO read every 30 ms, the buttons keep their sampling meanwhile.
#[embassy_executor::task]
async fn input(bus: I2cBus, mut button: ExtiInput<'static, Async>, int: HeaderInt, cfg: InputCfg) {
    let mut int = int.into_input();
    let mut keys = Keys::new(cfg.buttons, cfg.scan);
    let mut gestures = cfg.gestures;
    let mut decoder = Decoder::new();
    let mut swipe: Option<Swipe> = None;
    let mut traffic = false;
    let mut quiet_until = Instant::from_ticks(0);
    loop {
        let mut at = keys.next_at();
        if let Some(s) = &swipe {
            at = Some(at.map_or(s.next_read, |a| a.min(s.next_read)));
        }
        let irq = async {
            if gestures && swipe.is_none() {
                int.wait_for_low().await
            } else {
                core::future::pending().await
            }
        };
        match select4(keys.edge(&mut button), irq, CTRL.receive(), until(at)).await {
            Either4::Second(()) => {
                decoder.reset();
                let now = Instant::now();
                swipe = Some(Swipe {
                    start: now,
                    next_read: now,
                    buttons: false,
                });
            }
            Either4::Third(Ctrl::Gestures(on)) => {
                gestures = on;
                swipe = None;
                bus.raw(|b| {
                    let mut s = Apds9960::new(b);
                    if on {
                        s.start_gestures(&cfg.gesture)
                    } else {
                        s.power_down()
                    }
                })
                .ok();
                traffic = true;
            }
            Either4::Third(c) => keys.control(c),
            _ => {}
        }

        let now = Instant::now();
        // The swipe: read what the FIFO holds; decide once the sensor leaves gesture mode.
        if let Some(s) = &mut swipe
            && now >= s.next_read
        {
            traffic = true;
            let active = bus.raw(|b| {
                let mut a = Apds9960::new(b);
                let mut samples = [Sample::default(); 32];
                let n = a.read_fifo(&mut samples).unwrap_or(0);
                for x in &samples[..n] {
                    decoder.push(*x);
                }
                a.gesture_active().unwrap_or(false)
            });
            if active && now.saturating_duration_since(s.start) < GESTURE_MAX {
                s.next_read = now + GESTURE_POLL;
            } else {
                bus.raw(|b| Apds9960::new(b).clear_fifo()).ok();
                let trace = decoder.trace();
                let g = decoder.finish();
                let ignored = g.is_some() && (s.buttons || now < quiet_until);
                SWIPE_TRACE.signal(SwipeTrace {
                    ms: now.saturating_duration_since(s.start).as_millis() as u32,
                    trace,
                    gesture: g,
                    ignored,
                });
                if let Some(g) = g {
                    if ignored {
                        log::debug!(target: "lcd", "swipe {g} ignored (a hand at the buttons)");
                    } else {
                        let _ = EVENTS.try_send(Event::Gesture((cfg.map)(g)));
                    }
                }
                swipe = None;
            }
        }

        // The buttons, sampled on every pass.
        let (mask, used) = keys.sample(now, bus, &button);
        traffic |= used;
        if mask != 0 {
            quiet_until = now + SWIPE_QUIET;
            if let Some(s) = &mut swipe {
                s.buttons = true;
            }
        }
        if traffic && !keys.rec.needs_polling() && swipe.is_none() {
            traffic = false;
            bus.rest().await;
        }
    }
}

/// Page mode's input: the buttons only (no swipes: their decoding is kept out of the image, and
/// the sensor stays powered down).
#[embassy_executor::task]
async fn input_keys(
    bus: I2cBus,
    mut button: ExtiInput<'static, Async>,
    buttons: tower_hmi::input::Config,
    scan: Duration,
) {
    let mut keys = Keys::new(buttons, scan);
    let mut traffic = false;
    loop {
        if let Either3::Second(c) =
            select3(keys.edge(&mut button), CTRL.receive(), until(keys.next_at())).await
        {
            keys.control(c);
        }
        traffic |= keys.sample(Instant::now(), bus, &button).1;
        if traffic && !keys.rec.needs_polling() {
            traffic = false;
            bus.rest().await;
        }
    }
}

// ---- test hooks and the `lcd` shell subtree ----------------------------------------------------

/// Inject a press of `button` held `ms` milliseconds (see [`Lcd::simulate`]); callable without a
/// handle, from a shell command for instance.
pub fn simulate(button: Button, ms: u32) {
    let _ = CTRL.try_send(Ctrl::Simulate(button.mask(), ms));
}

/// Inject a swipe event, as if the gesture sensor had decoded it.
pub fn inject_swipe(gesture: Gesture) {
    let _ = EVENTS.try_send(Event::Gesture(gesture));
}

/// The driver's counters since boot.
pub fn stats() -> Stats {
    Stats {
        flushes: FLUSHES.load(Ordering::Relaxed),
        rows: ROWS_SENT.load(Ordering::Relaxed),
        vcom_toggles: VCOM_TOGGLES.load(Ordering::Relaxed),
    }
}

/// Shell commands for an `lcd` menu, a test bench's and a developer's view of the module. Mount
/// them in the app's command tree:
///
/// ```ignore
/// static CMDS: &[shell::Entry] = &[shell::Entry::menu("lcd", lcd::SHELL)];
/// app!(run, commands: CMDS);
/// ```
///
/// * `lcd press <left|right|both|core> [ms]`: a press held `ms` (default 100), recognized like a
///   real one (console only: over the radio it would let anyone drive the UI);
/// * `lcd swipe <up|down|left|right>`: a decoded swipe (console only);
/// * `lcd rotate <0|90|180|270>`: turn the screen as auto-rotation would, animated (console only;
///   needs [`autorotate`] running);
/// * `lcd stats`: flushes, rows sent, VCOM-only toggles;
/// * `lcd dump`: the frame, one row per line in hex (128 lines of 32 digits, a set bit ink);
///   `tools/lcd_dump.py` renders it to a PNG. Drawing pauses while it is paged out (about a
///   second), so an animated screen dumps whole.
pub static SHELL: &[crate::shell::Entry] = &[
    crate::shell::Entry::cmd_console_only("press", crate::shell::Args::None, cmd_press),
    crate::shell::Entry::cmd_console_only("rotate", crate::shell::Args::None, cmd_rotate),
    crate::shell::Entry::cmd_console_only("swipe", crate::shell::Args::None, cmd_swipe),
    crate::shell::Entry::cmd("stats", crate::shell::Args::None, cmd_stats),
    crate::shell::Entry::cmd("dump", crate::shell::Args::None, cmd_dump),
];

/// Longest press `lcd press` injects.
const PRESS_MAX_MS: u32 = 15_000;

fn cmd_press(ctx: &mut crate::shell::Ctx<'_>, args: &[&str]) -> crate::shell::Outcome {
    use core::fmt::Write as _;
    let button = match args.first().copied() {
        Some("left") => Some(Button::Left),
        Some("right") => Some(Button::Right),
        Some("both") => Some(Button::Both),
        Some("core") => Some(Button::Core),
        _ => None,
    };
    let ms = match args.get(1) {
        None => Some(100),
        Some(a) => a.parse::<u32>().ok().filter(|ms| (1..=PRESS_MAX_MS).contains(ms)),
    };
    match (button, ms) {
        (Some(b), Some(ms)) => {
            simulate(b, ms);
            let _ = write!(ctx, "{b} pressed for {ms} ms");
            crate::shell::Outcome::ok()
        }
        _ => {
            let _ = write!(
                ctx,
                "usage: lcd press <left|right|both|core> [ms 1..={PRESS_MAX_MS}]"
            );
            crate::shell::Outcome::code(crate::shell::R_BAD_ARG)
        }
    }
}

fn cmd_swipe(ctx: &mut crate::shell::Ctx<'_>, args: &[&str]) -> crate::shell::Outcome {
    use core::fmt::Write as _;
    let g = match args.first().copied() {
        Some("up") => Gesture::Up,
        Some("down") => Gesture::Down,
        Some("left") => Gesture::Left,
        Some("right") => Gesture::Right,
        _ => {
            let _ = write!(ctx, "usage: lcd swipe <up|down|left|right>");
            return crate::shell::Outcome::code(crate::shell::R_BAD_ARG);
        }
    };
    inject_swipe(g);
    let _ = write!(ctx, "swipe {g}");
    crate::shell::Outcome::ok()
}

fn cmd_rotate(ctx: &mut crate::shell::Ctx<'_>, args: &[&str]) -> crate::shell::Outcome {
    use core::fmt::Write as _;
    let r = match args.first().copied() {
        Some("0") => Rotation::R0,
        Some("90") => Rotation::R90,
        Some("180") => Rotation::R180,
        Some("270") => Rotation::R270,
        _ => {
            let _ = write!(ctx, "usage: lcd rotate <0|90|180|270>");
            return crate::shell::Outcome::code(crate::shell::R_BAD_ARG);
        }
    };
    if auto_rotate::request_turn(r) {
        let _ = write!(ctx, "turning to {}°", r as u16 * 90);
        crate::shell::Outcome::ok()
    } else {
        let _ = write!(ctx, "auto-rotation is not running");
        crate::shell::Outcome::code(crate::shell::R_BAD_ARG)
    }
}

fn cmd_stats(ctx: &mut crate::shell::Ctx<'_>, _args: &[&str]) -> crate::shell::Outcome {
    use core::fmt::Write as _;
    let s = stats();
    let _ = write!(
        ctx,
        "flushes: {}\nrows sent: {}\nvcom toggles: {}",
        s.flushes, s.rows, s.vcom_toggles
    );
    crate::shell::Outcome::ok()
}

/// How `lcd dump` reads the screen in the running mode (set by [`init`] or [`init_paged`]).
static DUMPER: BlockingMutex<ThreadModeRawMutex, Cell<Option<Dumper>>> = BlockingMutex::new(Cell::new(None));
type Dumper = fn(&mut crate::shell::Ctx<'_>) -> crate::shell::Outcome;

fn cmd_dump(ctx: &mut crate::shell::Ctx<'_>, _args: &[&str]) -> crate::shell::Outcome {
    use core::fmt::Write as _;
    match DUMPER.lock(|d| d.get()) {
        Some(dump) => dump(ctx),
        None => {
            let _ = write!(ctx, "the display is not running");
            crate::shell::Outcome::code(crate::shell::R_BAD_ARG)
        }
    }
}

/// One row of the dump: its bytes in hex.
fn dump_row(ctx: &mut crate::shell::Ctx<'_>, row: &[u8]) {
    use core::fmt::Write as _;
    for b in row {
        let _ = write!(ctx, "{b:02x}");
    }
    let _ = ctx.write_str("\n");
}

/// Frame mode's `lcd dump`: the frame.
fn dump_frame(ctx: &mut crate::shell::Ctx<'_>) -> crate::shell::Outcome {
    use core::fmt::Write as _;
    // Hold drawing for a moment: the host pages the dump out over several requests, each a run
    // of this handler, and they should all see the same frame (an animated screen redraws every
    // few tens of ms). Each run extends the hold; drawing resumes a second after the last.
    DUMP_HOLD.lock(|c| c.set(Instant::now() + Duration::from_millis(1000)));
    // A shell handler is synchronous: it takes the frame only if no flush holds it right now.
    let Ok(f) = FRAME.try_lock() else {
        let _ = write!(ctx, "busy, try again");
        return crate::shell::Outcome::code(crate::shell::R_BAD_ARG);
    };
    for y in 0..ROWS {
        dump_row(ctx, f.frame.row(y));
    }
    crate::shell::Outcome::ok()
}

/// Page mode's `lcd dump`: the renderer's screen, band by band into a buffer of its own (what
/// [`Lcd::show`] would send; the app's state may change between the requests a dump is paged out
/// over).
fn dump_paged(ctx: &mut crate::shell::Ctx<'_>) -> crate::shell::Outcome {
    let Some(render) = RENDER.lock(|r| r.get()) else {
        return crate::shell::Outcome::code(crate::shell::R_BAD_ARG);
    };
    let rotation = rotation_from(ROTATION.load(Ordering::Relaxed));
    // Rows `1 + STRIDE` apart with the pixels at `1..`, the layout `render_band` takes.
    let mut lines = [0u8; CHUNK_LINES * (1 + STRIDE)];
    for band in 0..BANDS {
        render_band(render, &mut lines, 1 + STRIDE, band, rotation);
        for l in 0..CHUNK_LINES {
            let at = l * (1 + STRIDE) + 1;
            dump_row(ctx, &lines[at..at + STRIDE]);
        }
    }
    crate::shell::Outcome::ok()
}
