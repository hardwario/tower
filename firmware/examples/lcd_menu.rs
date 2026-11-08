//! lcd_menu: a declarative settings menu on the TOWER LCD Module ([`hmi::menu`](tower::hmi::menu)),
//! its values kept as shell settings in EEPROM.
//!
//! A home screen shows the TMP112 temperature, measured every `period` seconds; at or above
//! `alarm` the screen says so and the LED blinks red, otherwise each measurement flashes the LED
//! in the chosen color. Hold the right button (or click both together, or swipe right) for the
//! menu: left/right click move, right hold enters or stores, left hold goes back; it returns home
//! after 30 s without input.
//!
//! The menu is `static` data: [`Menu`]s of [`Item`]s whose ids are the keys of the app's shell
//! [`Setting`](shell::Setting)s, edited through [`shell::SettingsValues`], so a change made on the
//! device shows in `tower exec "system settings print"` and a `system settings set led=blue` made
//! over the console shows on the device. The `Live` wrapper applies each change at once
//! (backlight, auto-rotation, rotation, swipes, LED). With `Auto-rotate` on (the default), the
//! screen follows the board's orientation with an animated turn.
//!
//!   just flash example lcd_menu

#![no_std]
#![no_main]

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use log::{error, info, warn};
use tower::hmi::menu::{Item, Menu, MenuState, MenuStyle, Values};
use tower::hmi::{Align, Color, Rect, Rotation, TextStyle, fonts};
use tower::lcd::{Button, Event, Gesture, Kind, MenuExit, Rgb};
use tower::shell::{self, Kind as SettingKind, Setting};
use tower::tmp112::{self, Tmp112};
use tower::{app, board::Board, board::UniqueId, i2c::I2cBus, lcd};

// ---- settings (shell keys from 0x10; the menu items use the same numbers as ids) ---------------

const BACKLIGHT: u8 = 0x10;
const LED: u8 = 0x11;
const PERIOD: u8 = 0x12;
const ALARM: u8 = 0x13;
const SWIPES: u8 = 0x14;
const ROTATION: u8 = 0x15;
const AUTOROTATE: u8 = 0x16;

/// A choice item and an enum setting must list their options in the same order: the menu value
/// is the index.
static LED_NAMES: &[&str] = &[
    "off", "red", "green", "blue", "yellow", "cyan", "magenta", "white",
];
static LED_COLORS: &[Rgb] = &[
    Rgb::OFF,
    Rgb::RED,
    Rgb::GREEN,
    Rgb::BLUE,
    Rgb::YELLOW,
    Rgb::CYAN,
    Rgb::MAGENTA,
    Rgb::WHITE,
];
static ROTATIONS: &[&str] = &["0", "90", "180", "270"];

static SETS: &[Setting] = &[
    Setting {
        key: BACKLIGHT,
        name: "backlight",
        kind: SettingKind::Bool,
        default: "false",
    },
    Setting {
        key: LED,
        name: "led",
        kind: SettingKind::Enum(LED_NAMES),
        default: "green",
    },
    Setting {
        key: PERIOD,
        name: "period",
        kind: SettingKind::Uint { min: 5, max: 600 },
        default: "30",
    },
    // Tenths of a degree Celsius.
    Setting {
        key: ALARM,
        name: "alarm",
        kind: SettingKind::Int { min: -200, max: 500 },
        default: "280",
    },
    Setting {
        key: SWIPES,
        name: "swipes",
        kind: SettingKind::Bool,
        default: "false",
    },
    Setting {
        key: ROTATION,
        name: "rotation",
        kind: SettingKind::Enum(ROTATIONS),
        default: "0",
    },
];

// ---- the menu ----------------------------------------------------------------------------------

const ABOUT: u16 = 1;
const HOME: u16 = 2;

static DISPLAY_MENU: Menu = Menu::new(
    "Display",
    &[
        Item::toggle("Backlight", BACKLIGHT as u16),
        Item::toggle("Auto-rotate", AUTOROTATE as u16),
        Item::choice("Rotation", ROTATION as u16, &["0°", "90°", "180°", "270°"]),
        Item::toggle("Swipes", SWIPES as u16),
        Item::back("Back"),
    ],
);

static MAIN_MENU: Menu = Menu::new(
    "Settings",
    &[
        Item::number("Period", PERIOD as u16, 5, 600, 5).unit(" s"),
        Item::number("Alarm", ALARM as u16, -200, 500, 5)
            .decimals(1)
            .unit("°C"),
        Item::choice(
            "LED",
            LED as u16,
            &[
                "Off", "Red", "Green", "Blue", "Yellow", "Cyan", "Magenta", "White",
            ],
        ),
        Item::submenu("Display", &DISPLAY_MENU),
        Item::action("About", ABOUT),
        Item::action("Home", HOME),
    ],
);

const MENU_STYLE: MenuStyle = MenuStyle::new(&fonts::SPLEEN_8X16);
const TITLE: TextStyle = TextStyle::new(&fonts::SPLEEN_8X16)
    .align(Align::Center)
    .inverted();
const READOUT: TextStyle = TextStyle::new(&fonts::SPLEEN_12X24).align(Align::Center);
const SMALL: TextStyle = TextStyle::new(&fonts::SPLEEN_6X12).align(Align::Center);

static CMDS: &[shell::Entry] = &[shell::Entry::menu("lcd", lcd::SHELL)];

/// The settings, applied to the module the moment the menu changes one.
struct Live {
    stored: shell::SettingsValues,
    lcd: lcd::Lcd,
    autorotation: lcd::AutoRotation,
}

impl Live {
    fn apply(&self, id: u16) {
        let v = self.stored.get(id);
        match id as u8 {
            BACKLIGHT => self.lcd.set_backlight(v != 0),
            AUTOROTATE => self.autorotation.set_enabled(v != 0),
            SWIPES => self.lcd.set_gestures(v != 0),
            ROTATION => self.lcd.set_rotation(match v {
                1 => Rotation::R90,
                2 => Rotation::R180,
                3 => Rotation::R270,
                _ => Rotation::R0,
            }),
            _ => {}
        }
    }
}

impl Values for Live {
    fn get(&self, id: u16) -> i32 {
        self.stored.get(id)
    }

    fn set(&mut self, id: u16, value: i32) {
        self.stored.set(id, value);
        info!(target: "menu", "{} set to {}", SETS.iter().find(|s| s.key as u16 == id).map_or("?", |s| s.name), value);
        self.apply(id);
    }
}

async fn run(b: Board) {
    let bus = I2cBus::share(b.tmp112.release());
    let mut tmp = Tmp112::new(bus, tmp112::ADDR_VPLUS);
    let res = lcd::Resources::new(b.header_spi, b.button, b.header_int);
    let lcd = match lcd::init(b.spawner, res, bus, lcd::Config::default()).await {
        Ok(lcd) => lcd,
        Err(e) => {
            error!(target: "lcd", "not started ({e:?}): is the LCD Module attached?");
            return;
        }
    };
    // Auto-rotation starts off; the `autorotate` setting (applied below) turns it on.
    let autorotate = lcd::AutoRotate {
        enabled: false,
        ..lcd::AutoRotate::default()
    };
    let autorotation = lcd::autorotate(b.spawner, lcd, b.accel_int, autorotate);
    let mut values = Live {
        stored: shell::SettingsValues::new(b.kv, SETS),
        lcd,
        autorotation,
    };
    for s in SETS {
        values.apply(s.key as u16);
    }

    let mut menu = MenuState::new(&MAIN_MENU);
    let mut tenths: Option<i32> = None;
    let mut next = Instant::now();
    loop {
        if Instant::now() >= next {
            tenths = match tmp.oneshot().await {
                Ok(raw) => Some(tmp112::raw_to_millicelsius(raw).div_euclid(100)),
                Err(e) => {
                    warn!(target: "tmp112", "read failed (I2C {e:?})");
                    None
                }
            };
            let alarm = tenths.is_some_and(|t| t >= values.get(ALARM as u16));
            let color = LED_COLORS
                .get(values.get(LED as u16) as usize)
                .copied()
                .unwrap_or(Rgb::OFF);
            match alarm {
                true => lcd.led().blink(Rgb::RED, 100, 100, 3),
                false if color != Rgb::OFF => lcd.led().blink(color, 30, 0, 1),
                false => {}
            }
            next = Instant::now() + Duration::from_secs(values.get(PERIOD as u16).max(5) as u64);
        }
        home(lcd, tenths, &values).await;

        let event = match select(Timer::at(next), lcd.next_event()).await {
            Either::First(()) => continue,
            Either::Second(e) => e,
        };
        let opens_menu = matches!(
            event,
            Event::Button(Button::Right, Kind::Hold)
                | Event::Button(Button::Both | Button::Core, Kind::Click)
                | Event::Gesture(Gesture::Right)
        );
        if !opens_menu {
            continue;
        }
        info!(target: "menu", "open");
        menu.reset();
        loop {
            match lcd
                .menu(&mut menu, &mut values, &MENU_STYLE, Some(Duration::from_secs(30)))
                .await
            {
                MenuExit::Action(ABOUT) => about(lcd).await,
                exit => {
                    info!(target: "menu", "closed ({exit:?})");
                    break;
                }
            }
        }
        // A measurement right away, with the new settings.
        next = Instant::now();
    }
}

/// The home screen: temperature, alarm threshold, measurement period.
async fn home(lcd: lcd::Lcd, tenths: Option<i32>, values: &Live) {
    let alarm = values.get(ALARM as u16);
    let period = values.get(PERIOD as u16);
    lcd.draw(|c| {
        let w = c.width();
        c.clear(Color::White);
        c.fill_rect(Rect::new(0, 0, w, 18), Color::Black);
        c.text(w / 2, 1, "Thermometer", &TITLE);
        match tenths {
            Some(t) => c.text_fmt(w / 2, 30, format_args!("{} °C", Tenths(t)), &READOUT),
            None => c.text(w / 2, 30, "-- °C", &READOUT),
        };
        if tenths.is_some_and(|t| t >= alarm) {
            let r = Rect::new(8, 60, w - 16, 16);
            c.fill_round_rect(r, 4, Color::Black);
            c.text_fmt(
                w / 2,
                62,
                format_args!("ALARM {} °C", Tenths(alarm)),
                &SMALL.inverted(),
            );
        } else {
            c.text_fmt(w / 2, 62, format_args!("alarm at {} °C", Tenths(alarm)), &SMALL);
        }
        c.text_fmt(w / 2, 78, format_args!("every {period} s"), &SMALL);
        c.hline(8, c.height() - 18, w - 16, Color::Black);
        c.text(w / 2, c.height() - 14, "hold right: menu", &SMALL);
    })
    .await;
}

/// The About page, until a button or a swipe.
async fn about(lcd: lcd::Lcd) {
    let s = lcd.stats();
    lcd.draw(|c| {
        let w = c.width();
        c.clear(Color::White);
        c.fill_rect(Rect::new(0, 0, w, 18), Color::Black);
        c.text(w / 2, 1, "About", &TITLE);
        let left = TextStyle::new(&fonts::SPLEEN_6X12);
        let up = Instant::now().as_secs();
        c.text(2, 24, "lcd_menu, TOWER SDK", &left);
        c.text_fmt(
            2,
            38,
            format_args!("{}", UniqueId::read()),
            &TextStyle::new(&fonts::SPLEEN_5X8),
        );
        c.text_fmt(
            2,
            50,
            format_args!("up {}:{:02}:{:02}", up / 3600, up / 60 % 60, up % 60),
            &left,
        );
        c.text_fmt(2, 64, format_args!("{} flushes", s.flushes), &left);
        c.text_fmt(2, 78, format_args!("{} rows sent", s.rows), &left);
        c.text_fmt(2, 92, format_args!("{} VCOM toggles", s.vcom_toggles), &left);
        c.text(w / 2, c.height() - 14, "any button: back", &SMALL);
    })
    .await;
    lcd.clear_events();
    while !matches!(
        lcd.next_event().await,
        Event::Button(_, Kind::Click | Kind::Hold) | Event::Gesture(_)
    ) {}
}

/// Tenths of a degree, shown with one decimal.
struct Tenths(i32);

impl core::fmt::Display for Tenths {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let a = self.0.unsigned_abs();
        write!(f, "{sign}{}.{}", a / 10, a % 10)
    }
}

app!(run, commands: CMDS, settings: SETS);
