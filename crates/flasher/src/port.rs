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
use std::time::Duration;

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
        let mut port = Port { inner };
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
    pub fn from_handle(inner: Box<dyn SerialPort>) -> Self {
        Port { inner }
    }

    /// Consume the `Port` and return the underlying serial handle.
    pub fn into_inner(self) -> Box<dyn SerialPort> {
        self.inner
    }

    pub fn set_timeout(&mut self, timeout: Duration) -> Result<()> {
        self.inner.set_timeout(timeout)?;
        Ok(())
    }

    /// Restore the default read timeout (after a temporary override).
    pub fn reset_timeout(&mut self) -> Result<()> {
        self.set_timeout(DEFAULT_TIMEOUT)
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
        self.set_rts(true)?;
        self.set_dtr(false)?; // RESET asserted, BOOT0 low
        sleep(RESET_PULSE);
        self.set_rts(false)?; // RESET released -> boot main flash
        if !settle.is_zero() {
            sleep(settle);
        }
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
    pub fn read_exact_buf(&mut self, buf: &mut [u8], context: &str) -> Result<()> {
        let mut filled = 0;
        while filled < buf.len() {
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
