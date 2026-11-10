//! The Relay Module: a latching (bistable) relay switched by an H-bridge on the pins of a
//! TCA9534A I/O expander. These are the expander's output patterns.
//!
//! Expander pins: P0 green LED, P1 red LED, P4 `RELAY_F_H`, P5 `RELAY_T_L`, P6 `RELAY_T_H`,
//! P7 `RELAY_F_L` (P2 and P3 are not connected). The high-side transistors are P-channel (on
//! while their pin is low), the low-side ones N-channel (on while high). A short coil pulse in the
//! "T" direction (`RELAY_T_H` and `RELAY_T_L` on) moves the contacts to one side, a pulse in the
//! "F" direction (`RELAY_F_H` and `RELAY_F_L` on) to the other; the coil is then released and the
//! contacts stay put, with no current and even without power.
//!
//! "On" is the T direction (the legacy SDK's `true`, the green LED), "off" the F direction (the
//! red LED).

/// The expander's addresses, in probe order: a TCA9534A with the default strapping (A2 low) and
/// with A2 high, then the same two strappings of a plain TCA9534 (some modules carry one).
pub const ADDRESSES: [u8; 4] = [0x3B, 0x3F, 0x23, 0x27];

/// The LED of the on (T) position.
pub const LED_GREEN: u8 = 1 << 0;
/// The LED of the off (F) position.
pub const LED_RED: u8 = 1 << 1;

const F_H: u8 = 1 << 4;
const T_L: u8 = 1 << 5;
const T_H: u8 = 1 << 6;
const F_L: u8 = 1 << 7;

/// All four transistors off: the high sides' pins high, the low sides' pins low. The coil draws
/// nothing and the expander's pins drive against their pull resistors only.
pub const IDLE: u8 = F_H | T_H;

/// Coil current towards `on`: for on, `RELAY_T_H` (P6 low) and `RELAY_T_L` (P5 high); for off,
/// `RELAY_F_H` (P4 low) and `RELAY_F_L` (P7 high). Hold it for [`PULSE_MS`], then write [`IDLE`].
pub const fn drive(on: bool) -> u8 {
    if on { F_H | T_L } else { T_H | F_L }
}

/// The LED of a position.
pub const fn led(on: bool) -> u8 {
    if on { LED_GREEN } else { LED_RED }
}

/// The coil pulse that moves the contacts, in milliseconds (the legacy SDK's, bench-proven).
pub const PULSE_MS: u32 = 20;
/// The least time between two coil pulses, in milliseconds: the module's reservoir capacitor
/// recharges meanwhile.
pub const RECHARGE_MS: u32 = 300;

#[cfg(test)]
mod tests {
    use super::*;

    /// Which transistors an output pattern turns on: (F_H, T_L, T_H, F_L).
    fn on(out: u8) -> (bool, bool, bool, bool) {
        (out & F_H == 0, out & T_L != 0, out & T_H == 0, out & F_L != 0)
    }

    #[test]
    fn idle_turns_every_transistor_off() {
        assert_eq!(on(IDLE), (false, false, false, false));
        assert_eq!(IDLE, 0b0101_0000);
    }

    #[test]
    fn a_drive_turns_on_one_diagonal_only() {
        assert_eq!(on(drive(true)), (false, true, true, false), "T: T_H and T_L");
        assert_eq!(on(drive(false)), (true, false, false, true), "F: F_H and F_L");
        assert_eq!(drive(true), 0b0011_0000);
        assert_eq!(drive(false), 0b1100_0000);
    }

    #[test]
    fn no_pattern_shorts_the_supply() {
        // A leg conducts from the supply to ground when its high side and its low side are both
        // on: the legs are (T_H, F_L) and (F_H, T_L).
        for led in [0, LED_GREEN, LED_RED] {
            for out in [IDLE | led, drive(true) | led, drive(false) | led] {
                let (f_h, t_l, t_h, f_l) = on(out);
                assert!(!(t_h && f_l), "leg T_H/F_L in {out:#010b}");
                assert!(!(f_h && t_l), "leg F_H/T_L in {out:#010b}");
            }
        }
    }

    #[test]
    fn the_leds_do_not_touch_the_bridge() {
        // P0 and P1, as the module is wired.
        assert_eq!((LED_GREEN, LED_RED), (0b01, 0b10));
        assert_eq!(led(true), LED_GREEN);
        assert_eq!(led(false), LED_RED);
        assert_eq!((LED_GREEN | LED_RED) & (F_H | T_L | T_H | F_L), 0);
    }
}
