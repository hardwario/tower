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
// order matters because the cap slows the NRST edge.
const BOOT_PRIME: Duration = Duration::from_millis(10);
const BOOT_RESET_HOLD: Duration = Duration::from_millis(50);
const BOOT_RELEASE_HOLD: Duration = Duration::from_millis(50);
const BOOT_PRE_INIT: Duration = Duration::from_millis(50);
const RESET_PULSE: Duration = Duration::from_millis(100);
const APP_BOOT: Duration = Duration::from_millis(150);
const RUN_SETTLE: Duration = Duration::from_millis(50);
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

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

    /// Pulse RESET with BOOT0 low so the chip boots the main flash application.
    pub fn reset_into_app(&mut self) -> Result<()> {
        self.set_rts(true)?;
        self.set_dtr(false)?; // RESET asserted, BOOT0 low
        sleep(RESET_PULSE);
        self.set_rts(false)?; // RESET released -> boot main flash
        sleep(APP_BOOT);
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
    pub fn read_available(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self.inner.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(0),
            Err(e) => Err(e.into()),
        }
    }
}
