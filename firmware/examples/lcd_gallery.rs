//! lcd_gallery: a tour of the [`hmi`](tower::hmi) drawing API on the TOWER LCD Module.
//!
//! Four pages: the bundled Spleen fonts, drawing primitives, 1-bit images (built from ASCII art
//! at compile time with `image!`), and text: alignment, Central European letters from a font
//! composed of Spleen blocks, symbols. Left and right click page back and forth; a right hold
//! turns the screen by 90 degrees.
//!
//!   just flash example lcd_gallery

#![no_std]
#![no_main]

use log::{error, info};
use tower::hmi::fonts::{self, spleen_8x16};
use tower::hmi::{Align, Canvas, Color, Font, Image, Rect, Rotation, TextStyle, image};
use tower::lcd::{Button, Event, Gesture, Kind};
use tower::{app, board::Board, i2c::I2cBus, lcd, shell};

static CMDS: &[shell::Entry] = &[shell::Entry::menu("lcd", lcd::SHELL)];

const TITLE: TextStyle = TextStyle::new(&fonts::SPLEEN_8X16)
    .align(Align::Center)
    .inverted();
const SMALL: TextStyle = TextStyle::new(&fonts::SPLEEN_6X12);

/// 8x16 with Latin Extended-A and the symbol block added: the linker keeps these blocks because
/// this font lists them, and nothing more.
static RICH: Font = spleen_8x16::with(&[
    &spleen_8x16::ASCII,
    &spleen_8x16::LATIN1,
    &spleen_8x16::LATIN_EXT_A,
    &spleen_8x16::SYMBOLS,
]);

const BELL: Image = image!(
    ".......##.......",
    "......####......",
    ".....######.....",
    "....########....",
    "....########....",
    "....########....",
    "...##########...",
    "..############..",
    ".##############.",
    "################",
    ".......##.......",
);

const BATTERY: Image = image!(
    ".#################..",
    "#.................#.",
    "#.###.###.###.....##",
    "#.###.###.###.....##",
    "#.###.###.###.....##",
    "#.................#.",
    ".#################..",
);

const SIGNAL: Image = image!(
    "...............##",
    "...............##",
    "...........##..##",
    "...........##..##",
    ".......##..##..##",
    ".......##..##..##",
    "...##..##..##..##",
    "...##..##..##..##",
    "##.##..##..##..##",
);

const CHECK: Image = image!(
    "..............##",
    ".............###",
    "............###.",
    "...........###..",
    "##........###...",
    "###......###....",
    ".###....###.....",
    "..###..###......",
    "...######.......",
    "....####........",
    ".....##.........",
);

const PAGES: usize = 4;

async fn run(b: Board) {
    let bus = I2cBus::share(b.tmp112.release());
    let res = lcd::Resources::new(b.header_spi, b.button, b.header_int);
    let lcd = match lcd::init(b.spawner, res, bus, lcd::Config::default()).await {
        Ok(lcd) => lcd,
        Err(e) => {
            error!(target: "lcd", "not started ({e:?}): is the LCD Module attached?");
            return;
        }
    };
    let mut page = 0;
    loop {
        lcd.draw(|c| {
            c.clear(Color::White);
            match page {
                0 => fonts_page(c),
                1 => shapes_page(c),
                2 => images_page(c),
                _ => text_page(c),
            }
        })
        .await;
        match lcd.next_event().await {
            Event::Button(Button::Right, Kind::Click) | Event::Gesture(Gesture::Left) => {
                page = (page + 1) % PAGES
            }
            Event::Button(Button::Left, Kind::Click) | Event::Gesture(Gesture::Right) => {
                page = (page + PAGES - 1) % PAGES
            }
            Event::Button(Button::Right, Kind::Hold) => {
                let next = match lcd.rotation() {
                    Rotation::R0 => Rotation::R90,
                    Rotation::R90 => Rotation::R180,
                    Rotation::R180 => Rotation::R270,
                    Rotation::R270 => Rotation::R0,
                };
                lcd.set_rotation(next);
                info!(target: "gallery", "rotation {next:?}");
            }
            _ => {}
        }
    }
}

fn title(c: &mut Canvas<'_>, text: &str) {
    let w = c.width();
    c.fill_rect(Rect::new(0, 0, w, 18), Color::Black);
    c.text(w / 2, 1, text, &TITLE);
}

fn fonts_page(c: &mut Canvas<'_>) {
    title(c, "Spleen fonts");
    let mut y = 20;
    for (font, sample) in [
        (&fonts::SPLEEN_5X8, "5x8 The quick brown fox"),
        (&fonts::SPLEEN_6X12, "6x12 jumps over the"),
        (&fonts::SPLEEN_8X16, "8x16 lazy dog"),
        (&fonts::SPLEEN_12X24, "12x24 °C"),
        (&fonts::SPLEEN_16X32, "16x32"),
    ] {
        c.text(2, y, sample, &TextStyle::new(font));
        y += font.height as i32 + 2;
    }
}

fn shapes_page(c: &mut Canvas<'_>) {
    title(c, "Primitives");
    c.rect(Rect::new(4, 24, 36, 26), Color::Black);
    c.fill_round_rect(Rect::new(46, 24, 36, 26), 7, Color::Black);
    c.round_rect(Rect::new(88, 24, 36, 26), 5, Color::Black);
    c.circle(22, 74, 16, Color::Black);
    c.fill_circle(64, 74, 16, Color::Black);
    c.fill_circle(64, 74, 7, Color::Invert);
    for i in 0..7 {
        c.line(90, 58, 90 + i * 5, 92, Color::Black);
    }
    // A bar graph, then the same area half inverted.
    for (i, h) in [6, 12, 9, 17, 22, 14, 19, 25].iter().enumerate() {
        c.fill_rect(Rect::new(6 + i as i32 * 9, 124 - h, 7, *h), Color::Black);
    }
    c.invert_rect(Rect::new(80, 98, 44, 26));
    c.text(102, 105, "XOR", &SMALL.align(Align::Center).color(Color::Invert));
}

fn images_page(c: &mut Canvas<'_>) {
    title(c, "Images");
    c.image(8, 28, &BELL, Color::Black, None);
    c.image(36, 30, &BATTERY, Color::Black, None);
    c.image(66, 29, &SIGNAL, Color::Black, None);
    c.image(96, 28, &CHECK, Color::Black, None);
    // White on black: the same images on an inked band.
    c.fill_rect(Rect::new(0, 50, c.width(), 22), Color::Black);
    c.image(8, 55, &BELL, Color::White, None);
    c.image(36, 57, &BATTERY, Color::White, None);
    c.image(66, 56, &SIGNAL, Color::White, None);
    c.image(96, 55, &CHECK, Color::White, None);
    c.text(2, 80, "image!(\"..##..\")", &SMALL);
    c.text(2, 94, "built at compile time", &SMALL);
    c.text(2, 108, "from ASCII art", &SMALL);
}

fn text_page(c: &mut Canvas<'_>) {
    title(c, "Text");
    let w = c.width();
    let base = TextStyle::new(&fonts::SPLEEN_6X12);
    c.text(2, 22, "left", &base);
    c.text(w / 2, 34, "center", &base.align(Align::Center));
    c.text(w - 2, 46, "right", &base.align(Align::Right));
    c.text(0, 60, "Příliš žluťoučký", &TextStyle::new(&RICH));
    c.text(2, 76, "kůň úpěl ódy", &TextStyle::new(&RICH));
    c.text(2, 92, "← ↑ → ↓ ■ □ ● ○", &TextStyle::new(&RICH));
    c.text(2, 108, "▁▂▃▄▅▆▇█▇▆▅▄▃▂▁", &TextStyle::new(&RICH));
}

app!(run, commands: CMDS);
