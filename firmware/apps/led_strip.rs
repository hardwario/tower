//! led_strip (TOWER IoT Kit product firmware): an addressable LED strip controller with live
//! control from the console shell.
//!
//! Drives the TOWER LED strip (144 SK6812 RGBW pixels on PA1; `NUM` and `KIND` below for
//! another strip) with the [`strip`](tower::strip) effects, played by
//! [`strip_player`](tower::strip_player) (shared with `radio_control`): rainbow, palettes, fire, twinkle,
//! candle, plasma, scanner, bouncing balls, meteor, sparkle, running lights, chase, wipe,
//! curtain, breathe, a solid colour, a sunrise wake-up light and a clock. Everything is
//! adjustable while it runs through the `strip` shell commands (`tower shell`, the console TUI,
//! or one-shot `tower exec "strip effect fire"`), and `strip save` keeps the settings in
//! EEPROM for the next power-up.
//!
//! The button: a **click** steps to the next effect, a **hold** turns the strip off and on.
//!
//! Commands (`strip ...`):
//!
//! * `status`: the effect and every setting, the strip's estimated current, the frame time and
//!   the driver's underrun count (late ring refills; should stay 0);
//! * `effect <name>`, `next`, `on`, `off`;
//! * `brightness <0-100>` (perceptual %), `color <name|rrggbb>` (for the one-colour effects),
//!   `palette <lava|ocean|forest|party|heat|sunrise>` (for `palette` and `plasma`),
//!   `speed <10-1000>` (% of normal);
//! * `gamma <linear|2.0|2.2|2.8>`, `dither <on|off>`, `reverse <on|off>` (strip mounted the
//!   other way), `limit <mA|off>` (the current cap, idle draw included);
//! * `time <hh:mm[:ss]>` (sets the clock; it counts from 00:00 at boot), `sunrise <minutes>`
//!   (starts a sunrise of that length);
//! * `save`: store the settings (effect, on/off, brightness, colour, palette, speed, gamma,
//!   dithering, orientation, limit, sunrise length) in EEPROM; they load at boot.
//!
//! Power: the strip needs its own 5 V supply sized for it (144 RGBW pixels can draw several
//! amperes at full white). The current limit defaults to 1500 mA; raise it only on a supply that
//! can take it.
//!
//!   just build app led_strip
//!   just run   app led_strip   (then `strip status` in the shell)

#![no_std]
#![no_main]

use embassy_futures::join::join;
use embassy_stm32::bind_interrupts;
use tower::shell::Entry;
use tower::strip::{LedKind, Strip};
use tower::{app, board::Board, button, strip_player, ws2812};

// The strip driver's refill interrupt (see `tower::ws2812::Refill`).
bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2_3 => ws2812::Refill;
});

/// Pixels on the strip.
const NUM: usize = 144;
/// Wire format: SK6812 RGBW (`LedKind::Rgb` for a WS2812B).
const KIND: LedKind = LedKind::Rgbw;

static COMMANDS: &[Entry] = &[strip_player::MENU];
static BUTTON: button::ButtonChannel = button::ButtonChannel::new();

async fn run(b: Board) {
    strip_player::load(b.kv);
    let btn = button::init_exti(
        b.spawner,
        b.button,
        button::Polarity::ActiveHigh,
        button::Config::default(),
        &BUTTON,
    );
    let mut strip = Strip::<NUM, { ws2812::rgbw_buf_len(NUM) }>::new(
        b.strip_tim,
        b.strip_data,
        b.strip_dma,
        Irqs,
        KIND,
        strip_player::brightness(),
    );
    let buttons = async {
        loop {
            match btn.next_event().await {
                button::Event::Click => {
                    strip_player::next_effect();
                }
                button::Event::Hold => strip_player::set_on(!strip_player::is_on()),
                _ => {}
            }
        }
    };
    log::info!(target: "strip", "click: next effect, hold: on/off");
    join(strip_player::play(&mut strip), buttons).await;
}

app!(run, commands: COMMANDS);
