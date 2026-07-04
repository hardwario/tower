//! Serial port wrapper: opens the bootloader UART (8-E-1) and drives RESET and
//! BOOT0 (through the bridge's RTS/DTR lines) to enter the bootloader or run the
//! application.
//!
//! On the HARDWARIO TOWER Radio Dongle the FT231X drives NRST and BOOT0 through
//! transistors, with a 1 µF cap on NRST so reset releases slowly. In raw
//! modem-line terms (`true` = line asserted) the verified behaviour is:
//!   (T,F) holds RESET asserted;
//!   (T,F) -> (F,T) raises BOOT0 while NRST ramps up (cap), so the chip latches
//!          BOOT0 high and boots system memory;
//!   (T,T) / (F,F) run the application.

use std::io::{self, Read, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};

use crate::error::{Error, Result};
use crate::target;

// Reset/BOOT0 timings for the HARDWARIO TOWER Radio Dongle (FT231X driving
// NRST/BOOT0 through transistors, 1 µF cap on NRST). The (rts, dtr) values in
// the sequences below are raw modem-line levels (true = line asserted); their
// order matters because the cap slows the NRST edge. They are `pub` so callers
// reconstructing the reset sequence over a foreign handle can reuse the exact
// tuned delays (see [`Port::from_handle`]).

/// Run-baseline settle before asserting RESET in the bootloader-entry sequence.
pub const BOOT_PRIME: Duration = Duration::from_millis(10);
/// How long RESET is held asserted before BOOT0 is raised.
pub const BOOT_RESET_HOLD: Duration = Duration::from_millis(50);
/// Settle after raising BOOT0 + releasing RESET (lets the NRST cap ramp up).
pub const BOOT_RELEASE_HOLD: Duration = Duration::from_millis(50);
/// Settle after dropping BOOT0, before the `0x7F` init byte.
pub const BOOT_PRE_INIT: Duration = Duration::from_millis(50);
/// RESET pulse width for a reset into the application.
pub const RESET_PULSE: Duration = Duration::from_millis(100);
/// Post-reset settle so the application has booted before we talk to it.
pub const APP_BOOT: Duration = Duration::from_millis(150);
/// Settle after driving the run-state lines on open.
pub const RUN_SETTLE: Duration = Duration::from_millis(50);
/// Default read timeout for the bootloader protocol.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

pub struct Port {
    inner: Box<dyn SerialPort>,
    /// Read timeout to restore on [`reset_timeout`](Port::reset_timeout).
    /// Captured the first time [`set_timeout`](Port::set_timeout) overrides the
    /// handle's timeout, so the engine's per-command overrides (e.g. the long
    /// erase timeout) are rolled back to whatever the handle actually had —
    /// notably a loaned handle's caller-configured timeout (see
    /// [`from_handle`](Port::from_handle)) — rather than to a hard-coded default.
    saved_timeout: Option<Duration>,
}

impl Port {
    /// Open the port at the bootloader baud, 8 data bits, even parity, 1 stop
    /// bit, no flow control. RTS/DTR are immediately driven to the run state
    /// because USB-UART bridges assert both lines on open.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_with(
            path,
            target::BAUD,
            DataBits::Eight,
            Parity::Even,
            StopBits::One,
        )
    }

    /// Open the port with a caller-specified frame format for general serial
    /// I/O (e.g. the monitor). Like [`open`](Self::open) it drives RTS/DTR to
    /// the run state `(T,T)` so the application keeps running rather than being
    /// held in reset by whatever level the bridge asserts on open.
    pub fn open_with(
        path: &str,
        baud: u32,
        data_bits: DataBits,
        parity: Parity,
        stop_bits: StopBits,
    ) -> Result<Self> {
        let inner = serialport::new(path, baud)
            .data_bits(data_bits)
            .parity(parity)
            .stop_bits(stop_bits)
            .flow_control(FlowControl::None)
            .timeout(DEFAULT_TIMEOUT)
            .open()?;
        let mut port = Port {
            inner,
            saved_timeout: None,
        };
        port.set_lines(true, true)?;
        sleep(RUN_SETTLE);
        Ok(port)
    }

    /// Wrap an already-open serial handle so callers can drive the tuned
    /// NRST/BOOT0 reset sequence (and the bootloader engine) without forking it.
    ///
    /// The caller is responsible for having opened the handle with the correct
    /// frame format — the bootloader protocol needs [`target::BAUD`] 8-E-1
    /// (see [`Port::open`]); a serial monitor picks its own format. Unlike
    /// [`open`](Self::open) / [`open_with`](Self::open_with) this does **not**
    /// drive the run-state lines or settle — it takes the handle exactly as
    /// given.
    ///
    /// The handle's configured read timeout is likewise preserved: the engine
    /// overrides it per command (e.g. the long erase timeout) but
    /// [`reset_timeout`](Self::reset_timeout) restores *this* handle's timeout,
    /// not a hard-coded default.
    pub fn from_handle(inner: Box<dyn SerialPort>) -> Self {
        Port {
            inner,
            saved_timeout: None,
        }
    }

    /// Consume the `Port` and return the underlying serial handle.
    pub fn into_inner(self) -> Box<dyn SerialPort> {
        self.inner
    }

    /// Override the read timeout, remembering the handle's current timeout the
    /// first time so [`reset_timeout`](Self::reset_timeout) can restore it.
    pub fn set_timeout(&mut self, timeout: Duration) -> Result<()> {
        if self.saved_timeout.is_none() {
            self.saved_timeout = Some(self.inner.timeout());
        }
        self.inner.set_timeout(timeout)?;
        Ok(())
    }

    /// Restore the read timeout that was in effect before the last
    /// [`set_timeout`](Self::set_timeout) override (falling back to
    /// [`DEFAULT_TIMEOUT`] if none was captured). Scoped rather than clobbering
    /// with the global default, so a loaned handle's timeout survives the
    /// engine's per-command overrides — see [`from_handle`](Self::from_handle).
    pub fn reset_timeout(&mut self) -> Result<()> {
        let restore = self.saved_timeout.take().unwrap_or(DEFAULT_TIMEOUT);
        self.inner.set_timeout(restore)?;
        Ok(())
    }

    /// Set the RTS line (`true` = asserted).
    fn set_rts(&mut self, level: bool) -> Result<()> {
        self.inner.write_request_to_send(level)?;
        Ok(())
    }

    /// Set the DTR line (`true` = asserted).
    fn set_dtr(&mut self, level: bool) -> Result<()> {
        self.inner.write_data_terminal_ready(level)?;
        Ok(())
    }

    /// Drive RTS/DTR together (RTS first).
    pub fn set_lines(&mut self, rts: bool, dtr: bool) -> Result<()> {
        self.set_rts(rts)?;
        self.set_dtr(dtr)?;
        Ok(())
    }

    /// Drive the chip into the system bootloader. The per-line write order is
    /// deliberate: with the 1 µF cap on NRST the transient ordering of each
    /// (rts,dtr) change matters.
    ///
    /// (T,F) asserts RESET; switching to (F,T) raises BOOT0 while RESET releases
    /// — the cap delays the NRST rise so BOOT0 is high when the chip latches it,
    /// selecting system memory. (F,F) then drops BOOT0 (harmless once latched).
    pub fn reset_into_bootloader(&mut self) -> Result<()> {
        self.clear_buffers()?;
        self.set_rts(true)?;
        self.set_dtr(true)?; // run baseline
        sleep(BOOT_PRIME);
        // From here NRST is driven low (RESET asserted). If a modem-line write
        // fails mid-sequence, don't leave the device held in reset: best-effort
        // restore the run state before surfacing the original error.
        self.drive_into_bootloader().inspect_err(|_| {
            let _ = self.set_lines(true, true);
        })
    }

    /// The RESET-asserted tail of [`reset_into_bootloader`](Self::reset_into_bootloader).
    /// Split out so its caller can restore the run state if any line write fails
    /// while NRST is held low.
    fn drive_into_bootloader(&mut self) -> Result<()> {
        self.set_rts(true)?;
        self.set_dtr(false)?; // RESET asserted
        sleep(BOOT_RESET_HOLD);
        self.set_dtr(true)?;
        self.set_rts(false)?; // BOOT0 high + RESET released (cap ramp)
        sleep(BOOT_RELEASE_HOLD);
        self.set_dtr(false)?; // BOOT0 low (already in bootloader)
        sleep(BOOT_PRE_INIT);
        self.clear_input()?;
        Ok(())
    }

    /// Pulse RESET with BOOT0 low so the chip boots the main flash application,
    /// then wait [`APP_BOOT`] for it to come up before returning.
    pub fn reset_into_app(&mut self) -> Result<()> {
        self.reset_into_app_settle(APP_BOOT)
    }

    /// Like [`reset_into_app`](Self::reset_into_app) but returns as soon as
    /// RESET is released, with no post-boot settle. Use when the caller manages
    /// the wait itself (e.g. it immediately starts streaming the app's output
    /// and wants the boot banner) — this is the reason tower-cli previously
    /// forked the reset sequence.
    pub fn reset_into_app_no_settle(&mut self) -> Result<()> {
        self.reset_into_app_settle(Duration::ZERO)
    }

    /// Pulse RESET with BOOT0 low so the chip boots the main flash application,
    /// waiting `settle` after releasing RESET.
    pub fn reset_into_app_settle(&mut self, settle: Duration) -> Result<()> {
        // If a line write fails mid-pulse with RESET possibly still asserted,
        // don't leave the device held in reset: best-effort restore the run
        // state before surfacing the original error.
        self.pulse_reset_into_app().inspect_err(|_| {
            let _ = self.set_lines(true, true);
        })?;
        if !settle.is_zero() {
            sleep(settle);
        }
        Ok(())
    }

    /// The RESET pulse of [`reset_into_app_settle`](Self::reset_into_app_settle),
    /// split out so its caller can restore the run state if a line write fails
    /// while NRST is asserted.
    fn pulse_reset_into_app(&mut self) -> Result<()> {
        self.set_rts(true)?;
        self.set_dtr(false)?; // RESET asserted, BOOT0 low
        sleep(RESET_PULSE);
        self.set_rts(false)?; // RESET released -> boot main flash
        Ok(())
    }

    pub fn clear_input(&mut self) -> Result<()> {
        self.inner.clear(ClearBuffer::Input)?;
        Ok(())
    }

    fn clear_buffers(&mut self) -> Result<()> {
        self.inner.clear(ClearBuffer::Input)?;
        self.inner.clear(ClearBuffer::Output)?;
        Ok(())
    }

    pub fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.inner.write_all(buf)?;
        self.inner.flush()?;
        Ok(())
    }

    /// Read exactly `buf.len()` bytes, honouring the configured read timeout.
    ///
    /// The serialport timeout is *per-`read`*, so it restarts on every call that
    /// returns data — a byte-at-a-time trickle would otherwise stretch an
    /// N-byte read without any wall-clock bound. So this also enforces an
    /// overall deadline scaled to the request size (one per-read timeout budget
    /// per byte, with the per-read timeout as a floor) and returns
    /// [`Error::Timeout`] once it is exceeded, regardless of trickle.
    pub fn read_exact_buf(&mut self, buf: &mut [u8], context: &str) -> Result<()> {
        let per_read = self.inner.timeout();
        let budget = per_read.saturating_mul(buf.len().max(1) as u32);
        let deadline = Instant::now() + budget;
        let mut filled = 0;
        while filled < buf.len() {
            if Instant::now() >= deadline {
                return Err(Error::Timeout {
                    context: context.to_string(),
                });
            }
            match self.inner.read(&mut buf[filled..]) {
                Ok(0) => {
                    return Err(Error::Timeout {
                        context: context.to_string(),
                    });
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    return Err(Error::Timeout {
                        context: context.to_string(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn read_byte(&mut self, context: &str) -> Result<u8> {
        let mut b = [0u8; 1];
        self.read_exact_buf(&mut b, context)?;
        Ok(b[0])
    }

    /// Read whatever bytes are currently available, up to `buf.len()`. Returns
    /// 0 on a read timeout (no data) instead of erroring — for streaming a port
    /// where idle gaps are normal rather than a failure (e.g. the monitor).
    ///
    /// Note: a genuine `Ok(0)` from the OS (EOF / device unplugged) is reported
    /// as [`Error::Disconnected`], not `Ok(0)` — otherwise a caller looping on
    /// this would busy-spin at 100% CPU after a USB disconnect. Only a *timeout*
    /// (the line is idle but still present) yields `Ok(0)`. See
    /// [`read_stream`](Self::read_stream) for the monitor's read loop.
    pub fn read_available(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.read_stream(buf, "reading serial port")
    }

    /// Streaming read for a serial monitor: `Ok(n>0)` for data, `Ok(0)` on an
    /// idle-line timeout, and [`Error::Disconnected`] when the OS reports EOF
    /// (an instantly-returning zero-byte read — the device was unplugged). This
    /// last case is what stops the read loop from spinning after a disconnect.
    pub fn read_stream(&mut self, buf: &mut [u8], context: &'static str) -> Result<usize> {
        match self.inner.read(buf) {
            Ok(0) => Err(Error::Disconnected { context }),
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(0),
            // A closed/unplugged device surfaces as one of these on some
            // platforms rather than a bare Ok(0).
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::NotConnected
                        | io::ErrorKind::UnexpectedEof
                ) =>
            {
                Err(Error::Disconnected { context })
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl crate::bootloader::Transport for Port {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        Port::write_all(self, buf)
    }

    fn read_exact_buf(&mut self, buf: &mut [u8], context: &str) -> Result<()> {
        Port::read_exact_buf(self, buf, context)
    }

    fn clear_input(&mut self) -> Result<()> {
        Port::clear_input(self)
    }

    fn set_timeout(&mut self, timeout: Duration) -> Result<()> {
        Port::set_timeout(self, timeout)
    }

    fn reset_timeout(&mut self) -> Result<()> {
        Port::reset_timeout(self)
    }

    fn read_byte(&mut self, context: &str) -> Result<u8> {
        Port::read_byte(self, context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Shared, inspectable state for [`FakePort`] — the test keeps a clone of
    /// the `Arc` to observe what the `Port` did to a moved-in handle.
    #[derive(Default)]
    struct FakeState {
        timeout: Duration,
        /// Recorded modem-line writes as `('R'|'D', level)`, in order. The
        /// failing write (see `fail_line_write_at`) is not recorded.
        lines: Vec<(char, bool)>,
        /// Count of modem-line writes attempted so far.
        line_writes: usize,
        /// 1-based index of the modem-line write that should return an error
        /// (to simulate a mid-sequence line-write failure), or `None`.
        fail_line_write_at: Option<usize>,
    }

    /// A scriptable in-memory [`SerialPort`] for exercising `Port`'s timeout
    /// bookkeeping, reset-line recovery, and read deadline without hardware.
    struct FakePort {
        state: Arc<Mutex<FakeState>>,
        /// Sleep before each `read` returns a byte — models a slow trickle.
        read_delay: Duration,
        /// Byte value each `read` yields.
        read_fill: u8,
    }

    impl FakePort {
        fn new(state: Arc<Mutex<FakeState>>) -> Self {
            FakePort {
                state,
                read_delay: Duration::ZERO,
                read_fill: 0,
            }
        }

        fn record_line(&mut self, which: char, level: bool) -> serialport::Result<()> {
            let mut s = self.state.lock().unwrap();
            s.line_writes += 1;
            if Some(s.line_writes) == s.fail_line_write_at {
                return Err(serialport::Error::new(
                    serialport::ErrorKind::Io(io::ErrorKind::Other),
                    "simulated modem-line write failure",
                ));
            }
            s.lines.push((which, level));
            Ok(())
        }
    }

    impl Read for FakePort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.read_delay.is_zero() {
                sleep(self.read_delay);
            }
            if buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.read_fill;
            Ok(1)
        }
    }

    impl Write for FakePort {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SerialPort for FakePort {
        fn name(&self) -> Option<String> {
            None
        }
        fn baud_rate(&self) -> serialport::Result<u32> {
            Ok(target::BAUD)
        }
        fn data_bits(&self) -> serialport::Result<DataBits> {
            Ok(DataBits::Eight)
        }
        fn flow_control(&self) -> serialport::Result<FlowControl> {
            Ok(FlowControl::None)
        }
        fn parity(&self) -> serialport::Result<Parity> {
            Ok(Parity::Even)
        }
        fn stop_bits(&self) -> serialport::Result<StopBits> {
            Ok(StopBits::One)
        }
        fn timeout(&self) -> Duration {
            self.state.lock().unwrap().timeout
        }
        fn set_baud_rate(&mut self, _: u32) -> serialport::Result<()> {
            Ok(())
        }
        fn set_data_bits(&mut self, _: DataBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_flow_control(&mut self, _: FlowControl) -> serialport::Result<()> {
            Ok(())
        }
        fn set_parity(&mut self, _: Parity) -> serialport::Result<()> {
            Ok(())
        }
        fn set_stop_bits(&mut self, _: StopBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
            self.state.lock().unwrap().timeout = timeout;
            Ok(())
        }
        fn write_request_to_send(&mut self, level: bool) -> serialport::Result<()> {
            self.record_line('R', level)
        }
        fn write_data_terminal_ready(&mut self, level: bool) -> serialport::Result<()> {
            self.record_line('D', level)
        }
        fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn bytes_to_read(&self) -> serialport::Result<u32> {
            Ok(0)
        }
        fn bytes_to_write(&self) -> serialport::Result<u32> {
            Ok(0)
        }
        fn clear(&self, _: ClearBuffer) -> serialport::Result<()> {
            Ok(())
        }
        fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
            Err(serialport::Error::new(
                serialport::ErrorKind::Unknown,
                "try_clone unsupported in tests",
            ))
        }
        fn set_break(&self) -> serialport::Result<()> {
            Ok(())
        }
        fn clear_break(&self) -> serialport::Result<()> {
            Ok(())
        }
    }

    fn port_with(state: Arc<Mutex<FakeState>>) -> Port {
        Port::from_handle(Box::new(FakePort::new(state)))
    }

    /// C2: a loaned handle's configured timeout survives the engine's per-command
    /// override — `reset_timeout` restores what `set_timeout` stashed, not the
    /// global `DEFAULT_TIMEOUT`.
    #[test]
    fn reset_timeout_restores_the_loaned_handles_timeout() {
        let caller_timeout = Duration::from_secs(7);
        let state = Arc::new(Mutex::new(FakeState {
            timeout: caller_timeout,
            ..Default::default()
        }));
        let mut port = port_with(state.clone());

        // Engine overrides the timeout for a slow command …
        port.set_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(state.lock().unwrap().timeout, Duration::from_secs(30));

        // … then rolls it back to the handle's own timeout, not DEFAULT_TIMEOUT.
        port.reset_timeout().unwrap();
        assert_eq!(state.lock().unwrap().timeout, caller_timeout);
        assert_ne!(caller_timeout, DEFAULT_TIMEOUT, "test would be vacuous");
    }

    /// Without a prior `set_timeout`, `reset_timeout` falls back to the global
    /// default (nothing was stashed to restore).
    #[test]
    fn reset_timeout_falls_back_to_default_when_nothing_stashed() {
        let state = Arc::new(Mutex::new(FakeState {
            timeout: Duration::from_secs(7),
            ..Default::default()
        }));
        let mut port = port_with(state.clone());
        port.reset_timeout().unwrap();
        assert_eq!(state.lock().unwrap().timeout, DEFAULT_TIMEOUT);
    }

    /// C3: if a modem-line write fails after RESET is asserted, the device is
    /// not left held in reset — the run state `(T,T)` is restored best-effort
    /// before the original error propagates.
    #[test]
    fn reset_into_bootloader_recovers_run_state_on_line_failure() {
        // Fail the 5th line write: set_rts(t), set_dtr(t) [run baseline],
        // set_rts(t), set_dtr(f) [RESET asserted], set_dtr(t) <- fails here.
        let state = Arc::new(Mutex::new(FakeState {
            timeout: DEFAULT_TIMEOUT,
            fail_line_write_at: Some(5),
            ..Default::default()
        }));
        let mut port = port_with(state.clone());
        let err = port.reset_into_bootloader().unwrap_err();
        assert!(matches!(err, Error::Serial(_)), "got {err:?}");

        let lines = &state.lock().unwrap().lines;
        assert_eq!(
            &lines[lines.len() - 2..],
            &[('R', true), ('D', true)],
            "run state restored after the failed write"
        );
    }

    /// C3: same guarantee for the reset-into-app pulse.
    #[test]
    fn reset_into_app_recovers_run_state_on_line_failure() {
        // Pulse writes: set_rts(t), set_dtr(f) [RESET asserted], set_rts(f)
        // [release] <- fail the 3rd, leaving the part held in reset.
        let state = Arc::new(Mutex::new(FakeState {
            timeout: DEFAULT_TIMEOUT,
            fail_line_write_at: Some(3),
            ..Default::default()
        }));
        let mut port = port_with(state.clone());
        let err = port.reset_into_app_no_settle().unwrap_err();
        assert!(matches!(err, Error::Serial(_)), "got {err:?}");

        let lines = &state.lock().unwrap().lines;
        assert_eq!(
            &lines[lines.len() - 2..],
            &[('R', true), ('D', true)],
            "run state restored after the failed write"
        );
    }

    /// C4: a slow trickle (each `read` returns a byte within the per-read
    /// timeout) cannot make an N-byte read run past the overall wall-clock
    /// deadline — it returns `Timeout` instead of blocking indefinitely.
    #[test]
    fn read_exact_buf_enforces_overall_deadline_under_trickle() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let mut fake = FakePort::new(state.clone());
        fake.read_delay = Duration::from_millis(10); // > the per-byte budget
        let mut port = Port::from_handle(Box::new(fake));

        // per-read timeout 1 ms, 4 bytes -> overall budget ~4 ms. Each 10 ms
        // read makes progress but blows the deadline after the first byte.
        port.set_timeout(Duration::from_millis(1)).unwrap();
        let mut buf = [0u8; 4];
        let err = port.read_exact_buf(&mut buf, "trickle").unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "got {err:?}");
    }

    /// A read that comfortably fits the budget still succeeds and fills the buf.
    #[test]
    fn read_exact_buf_succeeds_within_budget() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let mut fake = FakePort::new(state.clone());
        fake.read_fill = 0x79;
        let mut port = Port::from_handle(Box::new(fake));
        port.set_timeout(Duration::from_secs(1)).unwrap();
        let mut buf = [0u8; 4];
        port.read_exact_buf(&mut buf, "fast").unwrap();
        assert_eq!(buf, [0x79; 4]);
    }
}
