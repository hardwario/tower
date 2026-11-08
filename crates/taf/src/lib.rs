//! Hardware-free kernels of **taf**, the TOWER Application Framework.
//!
//! The firmware runtime (`tower::taf` in the SDK) does the I/O; the decisions it makes live here,
//! unit-tested on the host:
//!
//! - [`report`]: when a measurement is worth an uplink (a change past a delta, or a heartbeat).
//! - [`clock`]: wall-clock time kept as an offset over the node's crystal clock, set by the
//!   host's time sync; weekday and minute of day in local time.
//! - [`program`]: a weekly heating program (switch points per day) and the setpoint it gives.
//! - [`thermostat`]: the heating demand with hysteresis, and how the modes and a manual override
//!   pick the setpoint.
//!
//! Units follow the property tables in `tower_protocol::props`: temperatures in hundredths of a
//! degree Celsius (`centi`), setpoints and hysteresis in tenths (`deci`).

#![no_std]

pub mod clock;
pub mod program;
pub mod report;
pub mod thermostat;
