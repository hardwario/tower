//! A player of the LED strip effects: the render loop, its settings and the `strip` shell
//! commands, shared by the `led_strip` and `radio_control` apps (moved out of `led_strip`,
//! 2026-11-09).
//!
//! [`play`] renders the selected effect at 50 fps on a [`Strip`] (or idles at 10 Hz while the strip
//! is off, so the MCU can sleep). Everything it renders comes from atomics the `strip` commands
//! ([`MENU`]) and the setters below change while it runs; [`load`] and [`save`] keep them in
//! EEPROM (`NS_APP` local 0x40, the `strip save` record).

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, Ordering::Relaxed};

use embassy_time::{Duration, Instant, Ticker, Timer};
use log::{error, info};
use serde::{Deserialize, Serialize};

use crate::board;
use crate::shell::{Args, Ctx, Entry, Outcome, R_BAD_ARG, R_STORAGE};
use crate::storage::{NS_APP, Nv};
use crate::strip::{
    Balls, CYAN, Fire, Gamma, MAGENTA, RED, Rgb, Rng, Strip, WARM_WHITE, YELLOW, palette, parse_color,
};

/// One animation step at 100 % speed.
const STEP_MS: u64 = 20;
/// Frame period (50 fps; a 144-pixel RGBW frame renders well within it).
const FRAME_MS: u64 = 20;

/// The effects, by the names the shell takes.
pub const EFFECTS: [&str; 18] = [
    "rainbow", "palette", "fire", "twinkle", "candle", "plasma", "scanner", "balls", "meteor", "sparkle",
    "running", "chase", "wipe", "curtain", "breathe", "solid", "sunrise", "clock",
];
/// The palettes, in the order of [`palette::ALL`].
pub const PALETTES: [&str; 6] = ["lava", "ocean", "forest", "party", "heat", "sunrise"];
pub const GAMMAS: [&str; 4] = ["linear", "2.0", "2.2", "2.8"];
const ON_OFF: [&str; 2] = ["on", "off"];

// --- the live settings, shared by the shell commands and the render loop ----------------------

static EFFECT: AtomicU8 = AtomicU8::new(0);
static ON: AtomicBool = AtomicBool::new(true);
static BRIGHTNESS: AtomicU8 = AtomicU8::new(30);
static COLOR: AtomicU32 = AtomicU32::new(WARM_WHITE.to_hex());
static PALETTE: AtomicU8 = AtomicU8::new(0);
static SPEED: AtomicU16 = AtomicU16::new(100);
static GAMMA: AtomicU8 = AtomicU8::new(1); // 2.0
static DITHER: AtomicBool = AtomicBool::new(true);
static REVERSED: AtomicBool = AtomicBool::new(false);
static LIMIT_MA: AtomicU32 = AtomicU32::new(1500);
static SUNRISE_MIN: AtomicU16 = AtomicU16::new(30);
/// Time of day minus uptime, in seconds (mod one day): the clock.
static CLOCK_OFFSET: AtomicU32 = AtomicU32::new(0);
/// Restart the current effect's time (a new sunrise).
static RESTART: AtomicBool = AtomicBool::new(false);
// What the render loop reports back.
static CURRENT_MA: AtomicU32 = AtomicU32::new(0);
static FRAME_US: AtomicU32 = AtomicU32::new(0);
static EFFECT_US: AtomicU32 = AtomicU32::new(0);

/// The settings `strip save` stores (postcard, `NS_APP` local [`KEY_STATE`]).
#[derive(Serialize, Deserialize)]
struct Saved {
    effect: u8,
    on: bool,
    brightness: u8,
    color: u32,
    palette: u8,
    speed: u16,
    gamma: u8,
    dither: bool,
    reversed: bool,
    limit_ma: u32,
    sunrise_min: u16,
}

/// `NS_APP` local of the `strip save` record: clear of the taf runtime's keys (0x00, its pairing record)
/// when a taf app (`radio_control`) plays the strip.
const KEY_STATE: u8 = 0x40;

impl Saved {
    fn capture() -> Self {
        Self {
            effect: EFFECT.load(Relaxed),
            on: ON.load(Relaxed),
            brightness: BRIGHTNESS.load(Relaxed),
            color: COLOR.load(Relaxed),
            palette: PALETTE.load(Relaxed),
            speed: SPEED.load(Relaxed),
            gamma: GAMMA.load(Relaxed),
            dither: DITHER.load(Relaxed),
            reversed: REVERSED.load(Relaxed),
            limit_ma: LIMIT_MA.load(Relaxed),
            sunrise_min: SUNRISE_MIN.load(Relaxed),
        }
    }

    /// Apply a loaded record, clamping anything out of range to a valid value.
    fn apply(&self) {
        EFFECT.store(self.effect.min(EFFECTS.len() as u8 - 1), Relaxed);
        ON.store(self.on, Relaxed);
        BRIGHTNESS.store(self.brightness.min(100), Relaxed);
        COLOR.store(self.color & 0xFF_FFFF, Relaxed);
        PALETTE.store(self.palette.min(PALETTES.len() as u8 - 1), Relaxed);
        SPEED.store(self.speed.clamp(10, 1000), Relaxed);
        GAMMA.store(self.gamma.min(GAMMAS.len() as u8 - 1), Relaxed);
        DITHER.store(self.dither, Relaxed);
        REVERSED.store(self.reversed, Relaxed);
        LIMIT_MA.store(self.limit_ma, Relaxed);
        SUNRISE_MIN.store(self.sunrise_min.clamp(1, 240), Relaxed);
    }
}

/// Load the settings `strip save` stored, if any.
pub fn load(kv: Nv) {
    match kv.scope(NS_APP).get::<Saved>(KEY_STATE) {
        Ok(Some(s)) => {
            s.apply();
            info!(target: "strip", "settings loaded");
        }
        Ok(None) => {}
        Err(e) => error!(target: "strip", "settings not loaded, using the defaults: {e}"),
    }
}

// --- shell commands ------------------------------------------------------------------------

fn usage(ctx: &mut Ctx<'_>, text: &str) -> Outcome {
    let _ = write!(ctx, "usage: strip {text}");
    Outcome::code(R_BAD_ARG)
}

fn on_off(arg: Option<&&str>) -> Option<bool> {
    match arg.copied() {
        Some("on") => Some(true),
        Some("off") => Some(false),
        _ => None,
    }
}

fn cmd_status(ctx: &mut Ctx<'_>, _args: &[&str]) -> Outcome {
    let yes_no = |b: bool| if b { "on" } else { "off" };
    let effect = EFFECTS[EFFECT.load(Relaxed) as usize];
    let limit = LIMIT_MA.load(Relaxed);
    let tod = time_of_day();
    let frame_us = FRAME_US.load(Relaxed);
    let _ = writeln!(ctx, "effect {} ({})", effect, yes_no(ON.load(Relaxed)));
    let _ = writeln!(
        ctx,
        "brightness {} %, gamma {}, dither {}, reverse {}",
        BRIGHTNESS.load(Relaxed),
        GAMMAS[GAMMA.load(Relaxed) as usize],
        yes_no(DITHER.load(Relaxed)),
        yes_no(REVERSED.load(Relaxed)),
    );
    let _ = writeln!(
        ctx,
        "color {:06x}, palette {}, speed {} %",
        COLOR.load(Relaxed),
        PALETTES[PALETTE.load(Relaxed) as usize],
        SPEED.load(Relaxed),
    );
    let _ = write!(ctx, "current {} mA (limit ", CURRENT_MA.load(Relaxed));
    let _ = if limit == 0 {
        write!(ctx, "off")
    } else {
        write!(ctx, "{limit} mA")
    };
    let effect_us = EFFECT_US.load(Relaxed);
    let _ = writeln!(
        ctx,
        "), frame {}.{} ms (effect {}.{} ms), underruns {}, cuts {}",
        frame_us / 1000,
        frame_us % 1000 / 100,
        effect_us / 1000,
        effect_us % 1000 / 100,
        crate::ws2812::underruns(),
        crate::ws2812::cuts()
    );
    let _ = write!(
        ctx,
        "sunrise {} min, clock {:02}:{:02}:{:02}",
        SUNRISE_MIN.load(Relaxed),
        tod / 3600,
        tod / 60 % 60,
        tod % 60
    );
    Outcome::ok()
}

fn cmd_effect(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    match args.first().and_then(|a| EFFECTS.iter().position(|e| e == a)) {
        Some(i) => {
            EFFECT.store(i as u8, Relaxed);
            ON.store(true, Relaxed);
            let _ = write!(ctx, "effect {}", EFFECTS[i]);
            Outcome::ok()
        }
        None => {
            let _ = write!(ctx, "usage: strip effect <");
            for (i, e) in EFFECTS.iter().enumerate() {
                let _ = write!(ctx, "{}{}", if i > 0 { "|" } else { "" }, e);
            }
            let _ = write!(ctx, ">");
            Outcome::code(R_BAD_ARG)
        }
    }
}

/// Step to the next effect (and turn the strip on); the new effect's index.
pub fn next_effect() -> usize {
    let i = (EFFECT.load(Relaxed) as usize + 1) % EFFECTS.len();
    EFFECT.store(i as u8, Relaxed);
    ON.store(true, Relaxed);
    i
}

fn cmd_next(ctx: &mut Ctx<'_>, _args: &[&str]) -> Outcome {
    let _ = write!(ctx, "effect {}", EFFECTS[next_effect()]);
    Outcome::ok()
}

fn cmd_on(ctx: &mut Ctx<'_>, _args: &[&str]) -> Outcome {
    ON.store(true, Relaxed);
    let _ = write!(ctx, "on");
    Outcome::ok()
}

fn cmd_off(ctx: &mut Ctx<'_>, _args: &[&str]) -> Outcome {
    ON.store(false, Relaxed);
    let _ = write!(ctx, "off");
    Outcome::ok()
}

fn cmd_brightness(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    match args
        .first()
        .and_then(|a| a.parse::<u8>().ok())
        .filter(|b| *b <= 100)
    {
        Some(b) => {
            BRIGHTNESS.store(b, Relaxed);
            let _ = write!(ctx, "brightness {b} %");
            Outcome::ok()
        }
        None => usage(ctx, "brightness <0-100>"),
    }
}

fn cmd_color(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    match args.first().and_then(|a| parse_color(a)) {
        Some(c) => {
            COLOR.store(c.to_hex(), Relaxed);
            let _ = write!(ctx, "color {:06x}", c.to_hex());
            Outcome::ok()
        }
        None => usage(ctx, "color <name|rrggbb> (red, green, blue, warm, white, ...)"),
    }
}

fn cmd_palette(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    match args.first().and_then(|a| PALETTES.iter().position(|p| p == a)) {
        Some(i) => {
            PALETTE.store(i as u8, Relaxed);
            let _ = write!(ctx, "palette {}", PALETTES[i]);
            Outcome::ok()
        }
        None => usage(ctx, "palette <lava|ocean|forest|party|heat|sunrise>"),
    }
}

fn cmd_speed(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    match args
        .first()
        .and_then(|a| a.parse::<u16>().ok())
        .filter(|s| (10..=1000).contains(s))
    {
        Some(s) => {
            SPEED.store(s, Relaxed);
            let _ = write!(ctx, "speed {s} %");
            Outcome::ok()
        }
        None => usage(ctx, "speed <10-1000> (% of normal)"),
    }
}

fn cmd_gamma(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    match args.first().and_then(|a| GAMMAS.iter().position(|g| g == a)) {
        Some(i) => {
            GAMMA.store(i as u8, Relaxed);
            let _ = write!(ctx, "gamma {}", GAMMAS[i]);
            Outcome::ok()
        }
        None => usage(ctx, "gamma <linear|2.0|2.2|2.8>"),
    }
}

fn cmd_dither(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    match on_off(args.first()) {
        Some(on) => {
            DITHER.store(on, Relaxed);
            let _ = write!(ctx, "dither {}", if on { "on" } else { "off" });
            Outcome::ok()
        }
        None => usage(ctx, "dither <on|off>"),
    }
}

fn cmd_reverse(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    match on_off(args.first()) {
        Some(on) => {
            REVERSED.store(on, Relaxed);
            let _ = write!(ctx, "reverse {}", if on { "on" } else { "off" });
            Outcome::ok()
        }
        None => usage(ctx, "reverse <on|off>"),
    }
}

fn cmd_limit(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    let ma = match args.first().copied() {
        Some("off") => Some(0),
        Some(a) => a.parse::<u32>().ok().filter(|ma| (100..=20_000).contains(ma)),
        None => None,
    };
    match ma {
        Some(0) => {
            LIMIT_MA.store(0, Relaxed);
            let _ = write!(ctx, "limit off");
            Outcome::ok()
        }
        Some(ma) => {
            LIMIT_MA.store(ma, Relaxed);
            let _ = write!(ctx, "limit {ma} mA");
            Outcome::ok()
        }
        None => usage(ctx, "limit <100-20000 mA|off>"),
    }
}

fn cmd_time(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    let parsed = args.first().and_then(|a| {
        let mut parts = a.split(':').map(|p| p.parse::<u32>().ok());
        let (h, m) = (parts.next()??, parts.next()??);
        let s = match parts.next() {
            Some(s) => s?,
            None => 0,
        };
        (parts.next().is_none() && h < 24 && m < 60 && s < 60).then(|| h * 3600 + m * 60 + s)
    });
    match parsed {
        Some(tod) => {
            let up = (Instant::now().as_secs() % 86_400) as u32;
            CLOCK_OFFSET.store((tod + 86_400 - up) % 86_400, Relaxed);
            let _ = write!(
                ctx,
                "clock {:02}:{:02}:{:02}",
                tod / 3600,
                tod / 60 % 60,
                tod % 60
            );
            Outcome::ok()
        }
        None => usage(ctx, "time <hh:mm[:ss]>"),
    }
}

fn cmd_sunrise(ctx: &mut Ctx<'_>, args: &[&str]) -> Outcome {
    match args
        .first()
        .and_then(|a| a.parse::<u16>().ok())
        .filter(|m| (1..=240).contains(m))
    {
        Some(m) => {
            SUNRISE_MIN.store(m, Relaxed);
            EFFECT.store(
                EFFECTS.iter().position(|e| *e == "sunrise").unwrap_or(0) as u8,
                Relaxed,
            );
            ON.store(true, Relaxed);
            RESTART.store(true, Relaxed);
            let _ = write!(ctx, "sunrise over {m} min");
            Outcome::ok()
        }
        None => usage(ctx, "sunrise <1-240 minutes>"),
    }
}

fn cmd_save(ctx: &mut Ctx<'_>, _args: &[&str]) -> Outcome {
    match ctx.kv.raw().scope(NS_APP).set(KEY_STATE, &Saved::capture()) {
        Ok(()) => {
            let _ = write!(ctx, "saved");
            Outcome::ok()
        }
        Err(e) => {
            let _ = write!(ctx, "not saved: {e}");
            Outcome::code(R_STORAGE)
        }
    }
}

static STRIP_COMMANDS: &[Entry] = &[
    Entry::cmd("status", Args::None, cmd_status),
    Entry::cmd("effect", Args::Names(&EFFECTS), cmd_effect),
    Entry::cmd("next", Args::None, cmd_next),
    Entry::cmd("on", Args::None, cmd_on),
    Entry::cmd("off", Args::None, cmd_off),
    Entry::cmd("brightness", Args::None, cmd_brightness),
    Entry::cmd("color", Args::None, cmd_color),
    Entry::cmd("palette", Args::Names(&PALETTES), cmd_palette),
    Entry::cmd("speed", Args::None, cmd_speed),
    Entry::cmd("gamma", Args::Names(&GAMMAS), cmd_gamma),
    Entry::cmd("dither", Args::Names(&ON_OFF), cmd_dither),
    Entry::cmd("reverse", Args::Names(&ON_OFF), cmd_reverse),
    Entry::cmd("limit", Args::None, cmd_limit),
    Entry::cmd("time", Args::None, cmd_time),
    Entry::cmd("sunrise", Args::None, cmd_sunrise),
    Entry::cmd("save", Args::None, cmd_save),
];

/// The `strip` shell commands: mount them in the app's command tree.
pub const MENU: Entry = Entry::menu("strip", STRIP_COMMANDS);

/// The clock's time of day, in seconds since midnight: the wall clock when a taf node has synced
/// it, else the uptime plus the `strip time` offset.
fn time_of_day() -> u32 {
    if let Some(t) = crate::taf::local_time() {
        return u32::from(t.minute) * 60 + u32::from(t.second);
    }
    ((Instant::now().as_secs() % 86_400) as u32 + CLOCK_OFFSET.load(Relaxed)) % 86_400
}

// --- the setters an app drives (the `strip` commands use the same atomics) ----------------------

/// Whether the strip is on.
pub fn is_on() -> bool {
    ON.load(Relaxed)
}

/// Turn the strip on or off.
pub fn set_on(on: bool) {
    ON.store(on, Relaxed);
}

/// The effect's index in [`EFFECTS`].
pub fn effect() -> usize {
    EFFECT.load(Relaxed) as usize % EFFECTS.len()
}

/// Select effect `index` of [`EFFECTS`] (and turn the strip on), restarting it.
pub fn set_effect(index: usize) {
    if index < EFFECTS.len() {
        EFFECT.store(index as u8, Relaxed);
        ON.store(true, Relaxed);
        RESTART.store(true, Relaxed);
    }
}

/// The brightness, 0 to 100 (perceptual %).
pub fn brightness() -> u8 {
    BRIGHTNESS.load(Relaxed)
}

pub fn set_brightness(percent: u8) {
    BRIGHTNESS.store(percent.min(100), Relaxed);
}

/// The colour of the one-colour effects, `0xRRGGBB`.
pub fn color() -> u32 {
    COLOR.load(Relaxed)
}

pub fn set_color(rgb: u32) {
    COLOR.store(rgb & 0xFF_FFFF, Relaxed);
}

/// The animation speed, % of normal.
pub fn speed() -> u16 {
    SPEED.load(Relaxed)
}

pub fn set_speed(percent: u16) {
    SPEED.store(percent.clamp(10, 1000), Relaxed);
}

/// Store the settings in EEPROM (what `strip save` does).
pub fn save(kv: Nv) -> Result<(), crate::storage::Error> {
    kv.scope(NS_APP).set(KEY_STATE, &Saved::capture())
}

// --- the render loop -----------------------------------------------------------------------

/// The state of the effect that keeps one between frames.
enum EffectState<const N: usize> {
    None,
    Fire(Fire<N>),
    Balls(Balls<4>),
}

/// Render the strip forever (see the module docs).
pub async fn play<const N: usize, const B: usize>(strip: &mut Strip<'_, N, B>) -> ! {
    let mut rng = Rng::new(board::rand_u32());
    // The two effects with state of their own share one slot (one runs at a time): RAM.
    let mut state = EffectState::None;

    let mut ticker = Ticker::every(Duration::from_millis(FRAME_MS));
    let mut effect = usize::MAX; // none yet: the first frame logs the effect
    let mut was_on = !ON.load(Relaxed);
    let mut started = Instant::now();
    let mut last = started;
    let mut steps: u64 = 0; // elapsed ms x speed %, since the effect started
    let mut t_prev: u32 = 0; // the step of the previous frame
    info!(target: "strip", "{} pixels", N);

    loop {
        let on = ON.load(Relaxed);
        let now = Instant::now();
        let current = EFFECT.load(Relaxed) as usize % EFFECTS.len();
        // (Load then store, not swap: the M0+ has no atomic read-modify-write, and a restart
        // requested between the two is only a restart missed.)
        let restart = RESTART.load(Relaxed);
        if current != effect || restart {
            RESTART.store(false, Relaxed);
            effect = current;
            started = now;
            steps = 0;
            t_prev = 0;
            strip.clear();
            info!(target: "strip", "effect {}", EFFECTS[effect]);
        }
        if on != was_on {
            was_on = on;
            info!(target: "strip", "{}", if on { "on" } else { "off" });
            if on {
                // While off the loop waited on its own timer, and a Ticker catches up on every
                // tick it missed: without a reset it would run flat out for hours after a long off.
                ticker.reset();
            }
        }

        strip.render.brightness = BRIGHTNESS.load(Relaxed);
        strip.render.gamma = Gamma::ALL[GAMMA.load(Relaxed) as usize % Gamma::ALL.len()];
        strip.render.dither = DITHER.load(Relaxed);
        strip.render.reversed = REVERSED.load(Relaxed);
        strip.render.power_limit_ma = LIMIT_MA.load(Relaxed);

        let dt = now.duration_since(last).as_millis();
        last = now;
        steps += dt * SPEED.load(Relaxed) as u64;
        let t = (steps / (STEP_MS * 100)) as u32;
        // Steps since the last frame, for the effects that advance their state step by step
        // (fire, sparkle, the meteor's tail), so they follow `speed` too; capped after a stall.
        let advance = t.wrapping_sub(t_prev).min(8);
        t_prev = t;
        let color = Rgb::from_hex(COLOR.load(Relaxed));
        let pal = palette::ALL[PALETTE.load(Relaxed) as usize % palette::ALL.len()].1;

        let t0 = Instant::now();
        if !on {
            strip.clear();
        } else {
            match EFFECTS[effect] {
                "rainbow" => strip.rainbow(t),
                "palette" => strip.palette_scroll(pal, t),
                "fire" => {
                    if !matches!(state, EffectState::Fire(_)) {
                        state = EffectState::Fire(Fire::new());
                    }
                    if let EffectState::Fire(fire) = &mut state {
                        for _ in 0..advance {
                            fire.step(&mut rng);
                        }
                        fire.draw(strip);
                    }
                }
                "twinkle" => strip.twinkle(color, t, 48),
                "candle" => strip.candle(t),
                "plasma" => strip.plasma(pal, t),
                "scanner" => strip.scanner(color, t, 16),
                "balls" => {
                    if !matches!(state, EffectState::Balls(_)) {
                        state = EffectState::Balls(Balls::new(N, 1400, [RED, YELLOW, CYAN, MAGENTA]));
                    }
                    if let EffectState::Balls(balls) = &mut state {
                        balls.step((dt * SPEED.load(Relaxed) as u64 / 100) as u32);
                        balls.draw(strip);
                    }
                }
                "meteor" => {
                    for k in 0..advance {
                        strip.meteor(color, t - advance + 1 + k, 4, &mut rng);
                    }
                }
                "sparkle" => {
                    for _ in 0..advance {
                        strip.sparkle(color, &mut rng);
                    }
                }
                "running" => strip.running_lights(color, t),
                "chase" => strip.theater_chase(color, t / 4),
                "wipe" => strip.wipe_in_out(color, t),
                "curtain" => strip.curtain(color, t / 2),
                "breathe" => strip.breathe(color, t),
                "solid" => strip.fill(color),
                "sunrise" => {
                    // Real time, whatever the speed: progress over the set length.
                    let total = SUNRISE_MIN.load(Relaxed) as u64 * 60_000;
                    let ms = now.duration_since(started).as_millis();
                    strip.sunrise((ms * 65_535 / total).min(65_535) as u16);
                }
                _ => {
                    let tod = time_of_day();
                    strip.clock((tod / 3600) as u8, (tod / 60 % 60) as u8, (tod % 60) as u8);
                }
            }
        }

        EFFECT_US.store(t0.elapsed().as_micros() as u32, Relaxed);
        strip.show().await;
        FRAME_US.store(t0.elapsed().as_micros() as u32, Relaxed);
        CURRENT_MA.store(strip.current_ma(), Relaxed);

        // Off: nothing moves, so look for changes only ten times a second (the MCU can sleep).
        if on {
            ticker.next().await;
        } else {
            Timer::after_millis(100).await;
        }
    }
}
