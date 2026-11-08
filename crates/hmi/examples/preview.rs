//! Render sample screens to PNG files on the host, to check layouts without hardware.
//!
//!     cargo run -p tower-hmi --example preview -- <out-dir>
//!
//! Writes `fonts.png`, `shapes.png`, `menu.png` and `menu-edit.png`: the 128 x 128 frame, each
//! pixel scaled up 4 times, ink dark on a pale grey (roughly the memory LCD's look).

use std::io::Write;
use std::path::{Path, PathBuf};

use tower_hmi::fonts::{self, SPLEEN_5X8, SPLEEN_6X12, SPLEEN_8X16, SPLEEN_12X24, SPLEEN_32X64_NUMERIC};
use tower_hmi::menu::{Item, Menu, MenuState, MenuStyle, Nav, Table};
use tower_hmi::{Align, Color, Frame, Image, Rect, TextStyle, image};

type F = Frame<16, 128>;

const BELL: Image = image!(
    "......##......",
    ".....####.....",
    "....######....",
    "...########...",
    "...########...",
    "...########...",
    "..##########..",
    ".############.",
    "##############",
    "......##......",
);

static DISPLAY: Menu = Menu::new(
    "Display",
    &[
        Item::toggle("Backlight", 1),
        Item::choice("Rotation", 2, &["0°", "90°", "180°", "270°"]),
        Item::back("Back"),
    ],
);
static MAIN: Menu = Menu::new(
    "Settings",
    &[
        Item::submenu("Display", &DISPLAY),
        Item::number("Interval", 3, 1, 60, 1).unit(" min"),
        Item::number("Alarm", 4, -200, 500, 5).decimals(1).unit(" °C"),
        Item::choice("LED", 5, &["Off", "Red", "Green", "Blue"]),
        Item::toggle("Gestures", 6),
        Item::action("Measure now", 10),
        Item::action("About", 11),
        Item::action("Factory reset", 12),
    ],
);

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out).unwrap();

    let mut f = F::new();
    {
        let mut c = f.canvas();
        let mut y = 0;
        for (font, s) in [
            (&SPLEEN_5X8, "Spleen 5x8 ±µ°"),
            (&SPLEEN_6X12, "Spleen 6x12 Ünï"),
            (&SPLEEN_8X16, "Spleen 8x16"),
            (&SPLEEN_12X24, "12x24 °C"),
        ] {
            c.text(0, y, s, &TextStyle::new(font));
            y += font.height as i32 + 1;
        }
        let big = TextStyle::new(&SPLEEN_32X64_NUMERIC).align(Align::Center);
        c.text_fmt(64, y, format_args!("{:.1}°", 23.4), &big);
    }
    save(&f, &out.join("fonts.png"));

    let mut f = F::new();
    {
        let mut c = f.canvas();
        let title = TextStyle::new(&fonts::SPLEEN_8X16)
            .align(Align::Center)
            .inverted();
        c.fill_rect(Rect::new(0, 0, 128, 18), Color::Black);
        c.text(64, 1, "TOWER HMI", &title);
        c.rect(Rect::new(2, 22, 40, 30), Color::Black);
        c.fill_round_rect(Rect::new(46, 22, 36, 30), 8, Color::Black);
        c.round_rect(Rect::new(86, 22, 40, 30), 6, Color::Black);
        c.circle(22, 76, 18, Color::Black);
        c.fill_circle(64, 76, 14, Color::Black);
        c.fill_circle(64, 76, 6, Color::Invert);
        for i in 0..8 {
            c.line(90, 60, 90 + i * 5, 95, Color::Black);
        }
        c.image(6, 104, &BELL, Color::Black, None);
        let bar = Rect::new(26, 106, 98, 10);
        c.rect(bar, Color::Black);
        c.fill_rect(
            Rect::new(bar.x + 2, bar.y + 2, (bar.w - 4) * 62 / 100, bar.h - 4),
            Color::Black,
        );
        c.text(
            26 + 49,
            118,
            "62 %",
            &TextStyle::new(&SPLEEN_5X8).align(Align::Center),
        );
    }
    save(&f, &out.join("shapes.png"));

    let mut values = Table::new([(1, 0), (2, 0), (3, 15), (4, 215), (5, 2), (6, 1)]);
    let mut m = MenuState::new(&MAIN);
    let style = MenuStyle::new(&SPLEEN_8X16).title_font(&SPLEEN_8X16);
    m.handle(Nav::Next, &mut values);
    m.handle(Nav::Next, &mut values);
    let mut f = F::new();
    m.render(&mut f.canvas(), &values, &style);
    save(&f, &out.join("menu.png"));
    m.handle(Nav::Enter, &mut values);
    m.handle(Nav::Next, &mut values);
    let mut f = F::new();
    m.render(&mut f.canvas(), &values, &style);
    save(&f, &out.join("menu-edit.png"));
    m.handle(Nav::Back, &mut values);
    for _ in 0..5 {
        m.handle(Nav::Next, &mut values);
    }
    let mut f = F::new();
    m.render(&mut f.canvas(), &values, &MenuStyle::new(&SPLEEN_6X12));
    save(&f, &out.join("menu-small.png"));
    println!("wrote screens to {}", out.display());
}

/// Write the frame as an 8-bit grayscale PNG, scaled 4x (stored deflate blocks: no compressor
/// needed).
fn save(f: &F, path: &Path) {
    const S: usize = 4;
    let (w, h) = (128 * S, 128 * S);
    let mut raw = Vec::with_capacity((w + 1) * h);
    for y in 0..h {
        raw.push(0); // filter: none
        let row = f.row(y / S);
        for x in 0..w {
            let px = x / S;
            let ink = row[px / 8] & (0x80 >> (px % 8)) != 0;
            raw.push(if ink { 0x30 } else { 0xD8 });
        }
    }
    let mut z = vec![0x78, 0x01];
    for (i, chunk) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        z.push(last as u8);
        z.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        z.extend_from_slice(chunk);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    std::fs::File::create(path).unwrap().write_all(&png).unwrap();
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}
