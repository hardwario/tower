//! Auto-rotation: the Core Module's LIS2DH12 accelerometer keeps the LCD Module's picture upright.
//!
//! The accelerometer watches gravity on its own, in low-power mode at 10 Hz, and raises INT1
//! (PB6) only when the device turns into another portrait / landscape zone
//! ([`Lis2dh12::enable_orientation`]). The task sleeps on that edge, lets the hand settle, reads
//! the acceleration, picks the rotation with hysteresis ([`tower_hmi::turn::orientation`]) and turns
//! the screen, animated or at once. Lying flat keeps the rotation it had. Cost: the accelerometer's
//! few µA, and a wake-up per turn of the device.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_stm32::exti::ExtiInput;
use embassy_stm32::mode::Async;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer};
use tower_hmi::Rotation;

use super::{Animation, Lcd};
use crate::lis2dh12::{self, Accel, Lis2dh12};

/// Auto-rotation settings.
#[derive(Clone, Copy)]
pub struct AutoRotate {
    /// How the screen turns (`None`: at once, no snapshot RAM needed).
    pub animation: Option<Animation>,
    /// The accelerometer's reading in the panel's axes, milli-g: `x` to the right and `y` down as
    /// the unrotated picture reads (how the LCD Module sits over the Core Module's accelerometer;
    /// see [`mounting`]).
    pub mounting: fn(Accel) -> (i32, i32),
    /// After the accelerometer reports a new zone, wait this long for the hand to settle before
    /// reading which way up the device is.
    pub settle: Duration,
    /// Start enabled.
    pub enabled: bool,
}

impl Default for AutoRotate {
    fn default() -> Self {
        Self {
            animation: Some(Animation::default()),
            mounting,
            settle: Duration::from_millis(150),
            enabled: true,
        }
    }
}

/// The LCD Module on the Core Module: the accelerometer's axes as the panel sees them. The
/// accelerometer reads +1 g on Z with the display facing up, while the panel's frame (x right,
/// y down) has its third axis pointing into the screen: the two frames differ by half a turn about
/// Y, so the panel's x is the accelerometer's -x (verified on the bench: upright reads y = -1 g).
pub fn mounting(a: Accel) -> (i32, i32) {
    (-(a.x as i32), a.y as i32)
}

enum Ctrl {
    Enable(bool),
    Turn(Rotation),
}

static CTRL: Channel<CriticalSectionRawMutex, Ctrl, 2> = Channel::new();
static RUNNING: AtomicBool = AtomicBool::new(false);

/// A handle to the running auto-rotation (fire-and-forget methods).
#[derive(Clone, Copy)]
pub struct AutoRotation(());

impl AutoRotation {
    /// Follow the accelerometer (`true`), or leave the rotation as it is and power the
    /// accelerometer down (`false`).
    pub fn set_enabled(&self, on: bool) {
        let _ = CTRL.try_send(Ctrl::Enable(on));
    }
}

/// Start auto-rotation of `lcd`'s screen, with the Core Module's accelerometer interrupt line
/// (`b.accel_int`, PB6). The accelerometer (LIS2DH12 at `0x19`, on the LCD's I²C bus) is this
/// task's from now on: don't use its tilt interrupt alongside.
pub fn autorotate(
    spawner: Spawner,
    lcd: Lcd,
    accel_int: ExtiInput<'static, Async>,
    cfg: AutoRotate,
) -> AutoRotation {
    RUNNING.store(true, Ordering::Relaxed);
    spawner.spawn(task(lcd, accel_int, cfg).unwrap());
    AutoRotation(())
}

/// Turn the screen to `rotation` the way auto-rotation does (animated per its settings), from a
/// shell command: `false` when auto-rotation is not running.
pub fn request_turn(rotation: Rotation) -> bool {
    RUNNING.load(Ordering::Relaxed) && CTRL.try_send(Ctrl::Turn(rotation)).is_ok()
}

#[embassy_executor::task]
async fn task(lcd: Lcd, mut int: ExtiInput<'static, Async>, cfg: AutoRotate) {
    let mut enabled = cfg.enabled;
    if enabled {
        start(lcd, &cfg).await;
    } else {
        stop(lcd).await;
    }
    loop {
        let edge = async {
            if enabled {
                int.wait_for_high().await
            } else {
                core::future::pending().await
            }
        };
        match select(edge, CTRL.receive()).await {
            Either::First(()) => {
                // Clear the latch first (re-arming INT1), then let the hand settle.
                let _ = lcd
                    .bus
                    .raw(|b| Lis2dh12::new(b, lis2dh12::ADDR_DEFAULT).orientation_changed());
                lcd.bus.rest().await;
                Timer::after(cfg.settle).await;
                follow(lcd, &cfg).await;
            }
            Either::Second(Ctrl::Enable(on)) if on != enabled => {
                enabled = on;
                if on {
                    start(lcd, &cfg).await
                } else {
                    stop(lcd).await
                }
            }
            Either::Second(Ctrl::Turn(r)) => turn(lcd, &cfg, r).await,
            Either::Second(Ctrl::Enable(_)) => {}
        }
    }
}

async fn start(lcd: Lcd, cfg: &AutoRotate) {
    let ok = lcd.bus.raw(|b| {
        Lis2dh12::new(b, lis2dh12::ADDR_DEFAULT)
            .enable_orientation()
            .is_ok()
    });
    lcd.bus.rest().await;
    if ok {
        log::info!(target: "lcd", "auto-rotation on");
        follow(lcd, cfg).await;
    } else {
        log::error!(target: "lcd", "auto-rotation: no accelerometer (I2C)");
    }
}

async fn stop(lcd: Lcd) {
    let _ = lcd
        .bus
        .raw(|b| Lis2dh12::new(b, lis2dh12::ADDR_DEFAULT).power_down());
    lcd.bus.rest().await;
    log::info!(target: "lcd", "auto-rotation off");
}

/// Read the acceleration and turn the screen if it says so.
async fn follow(lcd: Lcd, cfg: &AutoRotate) {
    let reading = lcd.bus.raw(|b| Lis2dh12::new(b, lis2dh12::ADDR_DEFAULT).read());
    lcd.bus.rest().await;
    let Ok(a) = reading else { return };
    let (x, y) = (cfg.mounting)(a);
    let current = lcd.rotation();
    let want = tower_hmi::turn::orientation(x, y, current);
    log::debug!(
        target: "lcd",
        "orientation: x {} y {} z {} mg, panel ({x}, {y}): {}° (was {}°)",
        a.x,
        a.y,
        a.z,
        want as u16 * 90,
        current as u16 * 90
    );
    if want != current {
        turn(lcd, cfg, want).await;
    }
}

async fn turn(lcd: Lcd, cfg: &AutoRotate, to: Rotation) {
    match cfg.animation {
        Some(a) => lcd.rotate_animated(to, a).await,
        None => lcd.rotate(to).await,
    }
}
