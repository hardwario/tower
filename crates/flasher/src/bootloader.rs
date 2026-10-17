//! STM32 USART bootloader protocol engine (AN3155).

use std::time::Duration;

use crate::error::{Error, NackStage, Result};
use crate::target;

const ACK: u8 = 0x79;
const NACK: u8 = 0x1F;
const INIT: u8 = 0x7F;

/// The byte-level serial operations the bootloader engine needs. Implemented by
/// [`Port`](crate::port::Port) for a real device; a scripted in-memory
/// implementation in the tests exercises the protocol sequencing without
/// hardware (see [`Bootloader`], which is generic over this trait).
pub trait Transport {
    fn write_all(&mut self, buf: &[u8]) -> Result<()>;
    fn read_exact_buf(&mut self, buf: &mut [u8], context: &str) -> Result<()>;
    fn clear_input(&mut self) -> Result<()>;
    fn set_timeout(&mut self, timeout: Duration) -> Result<()>;
    fn reset_timeout(&mut self) -> Result<()>;

    fn read_byte(&mut self, context: &str) -> Result<u8> {
        let mut b = [0u8; 1];
        self.read_exact_buf(&mut b, context)?;
        Ok(b[0])
    }
}

const CMD_GET: u8 = 0x00;
const CMD_GET_ID: u8 = 0x02;
const CMD_READ_MEMORY: u8 = 0x11;
const CMD_GO: u8 = 0x21;
const CMD_WRITE_MEMORY: u8 = 0x31;
const CMD_EXT_ERASE: u8 = 0x44;
// Readout protection (see `crate::rdp` for the semantics and the sources). Each one is sent by
// exactly one crate-private method below, which only `rdp::protect` / `rdp::unprotect` call; a
// test in `rdp` pins that. Write Protect (0x63), Write Unprotect (0x73) and any option-byte
// write are deliberately absent.
const CMD_READOUT_PROTECT: u8 = 0x82;
const CMD_READOUT_UNPROTECT: u8 = 0x92;

/// AN3155 command frame: the byte followed by its complement.
fn cmd_frame(cmd: u8) -> [u8; 2] {
    [cmd, cmd ^ 0xFF]
}

/// Address frame: 4 bytes MSB-first followed by their XOR checksum.
fn address_frame(addr: u32) -> [u8; 5] {
    let b = addr.to_be_bytes();
    [b[0], b[1], b[2], b[3], b[0] ^ b[1] ^ b[2] ^ b[3]]
}

/// Write-Memory data frame: `N-1`, the data, then the checksum
/// (initialised to `N-1` and XORed with every data byte).
fn write_data_frame(data: &[u8]) -> Vec<u8> {
    let n = (data.len() - 1) as u8;
    let mut frame = Vec::with_capacity(data.len() + 2);
    frame.push(n);
    frame.extend_from_slice(data);
    let mut cs = n;
    for &b in data {
        cs ^= b;
    }
    frame.push(cs);
    frame
}

/// Extended Erase page-list frame (the bytes after `44 BB`): a big-endian
/// page count (N = pages-1), each page number big-endian, then the XOR checksum.
/// The STM32L0 bootloader rejects the 0xFFFF mass-erase special code, so erase
/// is always done with an explicit page list.
fn erase_frame(pages: &[u16]) -> Vec<u8> {
    let count = (pages.len() - 1) as u16;
    let mut f = Vec::with_capacity(2 + pages.len() * 2 + 1);
    f.push((count >> 8) as u8);
    f.push((count & 0xFF) as u8);
    for &p in pages {
        f.push((p >> 8) as u8);
        f.push((p & 0xFF) as u8);
    }
    let cs = f.iter().fold(0u8, |acc, &b| acc ^ b);
    f.push(cs);
    f
}

/// Parse the product id payload of a `Get ID` reply.
fn parse_chip_id(buf: &[u8]) -> u16 {
    match buf.len() {
        0 => 0,
        1 => buf[0] as u16,
        _ => ((buf[0] as u16) << 8) | buf[1] as u16,
    }
}

pub struct Bootloader<'a, T: Transport = crate::port::Port> {
    port: &'a mut T,
}

impl<'a, T: Transport> Bootloader<'a, T> {
    pub fn new(port: &'a mut T) -> Self {
        Bootloader { port }
    }

    fn read_ack(&mut self, context: &str, stage: NackStage) -> Result<()> {
        match self.port.read_byte(context)? {
            ACK => Ok(()),
            NACK => Err(Error::Nack {
                context: context.to_string(),
                stage,
            }),
            other => Err(Error::UnexpectedByte {
                context: context.to_string(),
                got: other,
                expected: ACK,
            }),
        }
    }

    fn send_command(&mut self, cmd: u8, context: &str) -> Result<()> {
        self.port.write_all(&cmd_frame(cmd))?;
        self.read_ack(context, NackStage::Command)
    }

    fn send_address(&mut self, addr: u32, context: &str) -> Result<()> {
        self.port.write_all(&address_frame(addr))?;
        self.read_ack(context, NackStage::Payload)
    }

    /// Auto-baud init: send a single `0x7F` and expect an ACK. Called after a
    /// fresh reset into the bootloader, so a NACK ("already initialised") means
    /// something is off; `connect` then retries with another reset.
    pub fn init(&mut self) -> Result<()> {
        self.port.clear_input()?;
        self.port.write_all(&[INIT])?;
        self.read_ack("bootloader init (0x7F)", NackStage::Command)
    }

    /// `Get ID` -> 16-bit product id (0x447 for STM32L0x3).
    pub fn get_id(&mut self) -> Result<u16> {
        self.send_command(CMD_GET_ID, "Get ID command")?;
        let n = self.port.read_byte("Get ID length")?;
        let mut buf = vec![0u8; n as usize + 1];
        self.port.read_exact_buf(&mut buf, "Get ID payload")?;
        self.read_ack("Get ID final ACK", NackStage::Payload)?;
        Ok(parse_chip_id(&buf))
    }

    /// `Get` -> (bootloader version, supported command bytes).
    pub fn get(&mut self) -> Result<(u8, Vec<u8>)> {
        self.send_command(CMD_GET, "Get command")?;
        let n = self.port.read_byte("Get length")?;
        let version = self.port.read_byte("Get bootloader version")?;
        let mut cmds = vec![0u8; n as usize];
        self.port.read_exact_buf(&mut cmds, "Get supported commands")?;
        self.read_ack("Get final ACK", NackStage::Payload)?;
        Ok((version, cmds))
    }

    /// Extended Erase of an explicit page list (1..=80 pages per call).
    /// `timeout` covers the (slow) erase ACK.
    ///
    /// The command byte + complement is acknowledged first (a NACK here is
    /// [`NackStage::Command`], usually line corruption); the page-list payload
    /// is acknowledged second (a NACK here is [`NackStage::Payload`], the
    /// device rejecting an out-of-range page). Callers walking an unknown flash
    /// density must treat only a *payload* NACK as the flash boundary.
    pub fn extended_erase_pages(&mut self, pages: &[u16], timeout: Duration) -> Result<()> {
        if pages.is_empty() || pages.len() > 80 {
            return Err(Error::InvalidArgument {
                context: "extended_erase_pages expects 1..=80 pages",
            });
        }
        self.send_command(CMD_EXT_ERASE, "Extended Erase command")?;
        self.port.set_timeout(timeout)?;
        let result = self
            .port
            .write_all(&erase_frame(pages))
            .and_then(|()| self.read_ack("Extended Erase pages", NackStage::Payload));
        let _ = self.port.reset_timeout();
        result
    }

    /// Write 1..=256 bytes at `addr`. Data should already be aligned/padded.
    ///
    /// The whole block must lie in main flash (`FLASH_BASE .. FLASH_BASE + MAX_FLASH_SIZE`).
    /// This bound is what keeps the option bytes (0x1FF8_0000 on the STM32L0, which hold the
    /// RDP level) and the data EEPROM out of reach of every caller, the public API included:
    /// writing RDP = 0xCC there would select level 2, which is permanent (RM0367 3.4.1,
    /// Table 19). Readout protection is changed only through [`crate::rdp`], and only between
    /// levels 0 and 1.
    pub fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        if data.is_empty() || data.len() > 256 {
            return Err(Error::InvalidArgument {
                context: "write_block expects 1..=256 bytes",
            });
        }
        let end = u64::from(addr) + data.len() as u64;
        let flash_end = u64::from(target::FLASH_BASE) + u64::from(target::MAX_FLASH_SIZE);
        if addr < target::FLASH_BASE || end > flash_end {
            return Err(Error::InvalidArgument {
                context: "write_block writes main flash only (never the option bytes or the data EEPROM)",
            });
        }
        self.send_command(CMD_WRITE_MEMORY, "Write Memory command")?;
        self.send_address(addr, "Write Memory address")?;
        self.port.write_all(&write_data_frame(data))?;
        self.read_ack("Write Memory data", NackStage::Payload)
    }

    /// Read `len` (1..=256) bytes from `addr`.
    pub fn read_block(&mut self, addr: u32, len: usize) -> Result<Vec<u8>> {
        if !(1..=256).contains(&len) {
            return Err(Error::InvalidArgument {
                context: "read_block expects 1..=256 bytes",
            });
        }
        self.send_command(CMD_READ_MEMORY, "Read Memory command")?;
        self.send_address(addr, "Read Memory address")?;
        let n = (len - 1) as u8;
        self.port.write_all(&[n, n ^ 0xFF])?;
        self.read_ack("Read Memory length", NackStage::Payload)?;
        let mut buf = vec![0u8; len];
        self.port.read_exact_buf(&mut buf, "Read Memory data")?;
        Ok(buf)
    }

    /// Jump to the application at `addr`.
    pub fn go(&mut self, addr: u32) -> Result<()> {
        self.send_command(CMD_GO, "Go command")?;
        self.send_address(addr, "Go address")
    }

    /// Wait `settle`, then drop whatever the bootloader has sent since. Used after a NACK
    /// that may be followed by a second one: some ST bootloaders send two NACKs for a Read or
    /// Write Memory refused by active readout protection (AN2606, STM32L1xxxC known
    /// limitations), and a stray NACK left in the buffer would read as the next command's
    /// ACK stage.
    pub(crate) fn discard_input(&mut self, settle: Duration) -> Result<()> {
        std::thread::sleep(settle);
        self.port.clear_input()
    }

    /// AN3155 Readout Protect (0x82): select RDP level 1. Crate-private on purpose: the only
    /// caller is [`crate::rdp::protect`], which probes first and verifies afterwards.
    ///
    /// Host side: send `82 7D`, wait for the first ACK (the command is accepted; a NACK here is
    /// [`NackStage::Command`]: line corruption, or protection already active, since Readout
    /// Protect is not among the commands the bootloader serves at level 1), then call
    /// `accepted` and wait up to `timeout` for the second ACK, which the bootloader sends after
    /// writing the option bytes (a NACK there is [`NackStage::Operation`]). The device then
    /// performs a system reset to load the new option bytes, so this session is over either
    /// way.
    pub(crate) fn readout_protect(&mut self, timeout: Duration, accepted: &mut dyn FnMut()) -> Result<()> {
        self.send_command(CMD_READOUT_PROTECT, "Readout Protect command")?;
        accepted();
        self.await_second_ack("Readout Protect option-byte update", timeout)
    }

    /// AN3155 Readout Unprotect (0x92): RDP level 1 back to level 0. The regression
    /// mass-erases the flash AND the data EEPROM (RM0367 3.4.1). Crate-private on purpose: the
    /// only caller is [`crate::rdp::unprotect`].
    ///
    /// Same handshake as [`readout_protect`](Self::readout_protect) (`92 6D`, first ACK,
    /// `accepted`, second ACK after the mass erase and the option-byte update, then a system
    /// reset); per AN3155, a NACK in place of the second ACK means the protection stays
    /// active.
    pub(crate) fn readout_unprotect(&mut self, timeout: Duration, accepted: &mut dyn FnMut()) -> Result<()> {
        self.send_command(CMD_READOUT_UNPROTECT, "Readout Unprotect command")?;
        accepted();
        self.await_second_ack("Readout Unprotect mass erase and option-byte update", timeout)
    }

    /// The second ACK of the readout-protection commands, under a long `timeout` (the device
    /// is busy with the option bytes and, for Unprotect, a mass erase). The default timeout is
    /// restored on both outcomes.
    fn await_second_ack(&mut self, context: &str, timeout: Duration) -> Result<()> {
        self.port.set_timeout(timeout)?;
        let result = self.read_ack(context, NackStage::Operation);
        let _ = self.port.reset_timeout();
        result
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// In-memory [`Transport`] for protocol tests (no hardware).
    ///
    /// Two ways to drive it, which compose:
    ///   * a **manual reply queue** (`push_ack`, `push_nack`, `push_bytes`, …):
    ///     reads pop these bytes in order, for deterministic scripts;
    ///   * an optional **erase-boundary simulation** (`set_erase_boundary`):
    ///     when set, Extended Erase (0x44) command+page-list frames are answered
    ///     automatically: the command byte ACKs, and the page list ACKs iff
    ///     every page is `< boundary` (erased pages are recorded), else NACKs.
    ///     This models a part of unknown density and lets [`erase_chip`]'s
    ///     bisection run against a moving boundary.
    ///
    /// All written bytes are recorded (`take_written`).
    pub struct ScriptedTransport {
        written: Vec<u8>,
        /// Scripted reply bytes; the flag marks a byte `clear_input` discards (a stray byte
        /// already in the receive buffer, see `push_stale`).
        replies: VecDeque<(u8, bool)>,
        timeouts: Vec<Duration>,
        erase_boundary: Option<usize>,
        erased_pages: Vec<u16>,
        // True once the current Extended Erase command byte has been consumed
        // and we're expecting its page-list payload next.
        awaiting_erase_pages: bool,
    }

    impl ScriptedTransport {
        pub fn new() -> Self {
            ScriptedTransport {
                written: Vec::new(),
                replies: VecDeque::new(),
                timeouts: Vec::new(),
                erase_boundary: None,
                erased_pages: Vec::new(),
                awaiting_erase_pages: false,
            }
        }

        /// Enable the Extended Erase simulation: pages `< boundary` erase (ACK),
        /// any page `>= boundary` NACKs the whole command (AN3155 semantics).
        pub fn set_erase_boundary(&mut self, boundary: usize) {
            self.erase_boundary = Some(boundary);
        }

        pub fn erased_pages(&self) -> &[u16] {
            &self.erased_pages
        }

        pub fn push_ack(&mut self) {
            self.replies.push_back((ACK, false));
        }

        pub fn push_nack(&mut self) {
            self.replies.push_back((NACK, false));
        }

        pub fn push_byte(&mut self, b: u8) {
            self.replies.push_back((b, false));
        }

        pub fn push_bytes(&mut self, bytes: &[u8]) {
            self.replies.extend(bytes.iter().map(|&b| (b, false)));
        }

        /// Queue a byte the device sends unasked (e.g. the second NACK some bootloaders send
        /// for a Read Memory refused by readout protection). A read still returns it, but a
        /// `clear_input` reaching it first drops it, like a real port's receive buffer.
        pub fn push_stale(&mut self, b: u8) {
            self.replies.push_back((b, true));
        }

        /// Queue an Extended Erase chunk reply: command-frame ACK + page-list ACK.
        pub fn push_erase_chunk_ack(&mut self) {
            self.push_ack();
            self.push_ack();
        }

        /// Queue a NACK to a command frame (line corruption of the command byte).
        pub fn push_command_nack(&mut self) {
            self.push_nack();
        }

        pub fn timeouts(&self) -> &[Duration] {
            &self.timeouts
        }

        /// All bytes written to the transport, in order.
        pub fn written(&self) -> &[u8] {
            &self.written
        }

        /// Parse a page-list frame (the bytes after `44 BB`) into page numbers.
        fn parse_page_list(frame: &[u8]) -> Vec<u16> {
            // [count_hi, count_lo, (page_hi, page_lo)*, checksum]
            let n = ((frame[0] as usize) << 8 | frame[1] as usize) + 1;
            let mut pages = Vec::with_capacity(n);
            for i in 0..n {
                let hi = frame[2 + i * 2] as u16;
                let lo = frame[2 + i * 2 + 1] as u16;
                pages.push((hi << 8) | lo);
            }
            pages
        }
    }

    impl Transport for ScriptedTransport {
        fn write_all(&mut self, buf: &[u8]) -> Result<()> {
            self.written.extend_from_slice(buf);

            if let Some(boundary) = self.erase_boundary {
                // Detect an Extended Erase command frame [0x44, 0xBB].
                if buf == cmd_frame(CMD_EXT_ERASE) {
                    self.replies.push_back((ACK, false)); // command byte accepted
                    self.awaiting_erase_pages = true;
                    return Ok(());
                }
                if self.awaiting_erase_pages {
                    self.awaiting_erase_pages = false;
                    let pages = Self::parse_page_list(buf);
                    if pages.iter().all(|&p| (p as usize) < boundary) {
                        for p in pages {
                            if !self.erased_pages.contains(&p) {
                                self.erased_pages.push(p);
                            }
                        }
                        self.replies.push_back((ACK, false));
                    } else {
                        self.replies.push_back((NACK, false));
                    }
                    return Ok(());
                }
            }
            Ok(())
        }

        fn read_exact_buf(&mut self, buf: &mut [u8], context: &str) -> Result<()> {
            for slot in buf.iter_mut() {
                *slot = self
                    .replies
                    .pop_front()
                    .map(|(b, _)| b)
                    .ok_or_else(|| Error::Timeout {
                        context: context.to_string(),
                    })?;
            }
            Ok(())
        }

        fn clear_input(&mut self) -> Result<()> {
            while self.replies.front().is_some_and(|&(_, stale)| stale) {
                self.replies.pop_front();
            }
            Ok(())
        }

        fn set_timeout(&mut self, timeout: Duration) -> Result<()> {
            self.timeouts.push(timeout);
            Ok(())
        }

        fn reset_timeout(&mut self) -> Result<()> {
            self.timeouts.push(DEFAULT_TIMEOUT_MARKER);
            Ok(())
        }
    }

    /// Sentinel recorded by `ScriptedTransport::reset_timeout` so tests can
    /// assert the default timeout was restored without depending on port.rs.
    pub const DEFAULT_TIMEOUT_MARKER: Duration = Duration::from_millis(1000);

    #[test]
    fn get_id_two_byte_reply() {
        // [ACK(cmd), len=1, 0x04, 0x47, ACK(final)] => 0x447
        let mut t = ScriptedTransport::new();
        t.push_bytes(&[ACK, 0x01, 0x04, 0x47, ACK]);
        let mut bl = Bootloader::new(&mut t);
        assert_eq!(bl.get_id().unwrap(), 0x447);
    }

    #[test]
    fn get_id_one_byte_reply() {
        // len=0 => payload is 1 byte (n+1). Reply: [ACK, 0, 0x12, ACK] => 0x12.
        let mut t = ScriptedTransport::new();
        t.push_bytes(&[ACK, 0x00, 0x12, ACK]);
        let mut bl = Bootloader::new(&mut t);
        assert_eq!(bl.get_id().unwrap(), 0x12);
    }

    #[test]
    fn read_ack_distinguishes_nack_and_garbage() {
        // NACK on the command frame -> Nack{stage: Command}.
        let mut t = ScriptedTransport::new();
        t.push_byte(0x1F);
        let mut bl = Bootloader::new(&mut t);
        let err = bl.get_id().unwrap_err();
        assert!(matches!(
            err,
            Error::Nack {
                stage: NackStage::Command,
                ..
            }
        ));

        // Garbage byte -> UnexpectedByte.
        let mut t = ScriptedTransport::new();
        t.push_byte(0xAB);
        let mut bl = Bootloader::new(&mut t);
        let err = bl.get_id().unwrap_err();
        assert!(matches!(
            err,
            Error::UnexpectedByte {
                got: 0xAB,
                expected: ACK,
                ..
            }
        ));
    }

    #[test]
    fn extended_erase_restores_timeout_on_ok_and_err() {
        // OK path: set(erase_timeout) then reset(default).
        let mut t = ScriptedTransport::new();
        t.push_erase_chunk_ack();
        let erase_timeout = Duration::from_secs(30);
        {
            let mut bl = Bootloader::new(&mut t);
            bl.extended_erase_pages(&[0, 1, 2], erase_timeout).unwrap();
        }
        assert_eq!(t.timeouts(), &[erase_timeout, DEFAULT_TIMEOUT_MARKER]);

        // Err path (page-list NACK): timeout still restored.
        let mut t = ScriptedTransport::new();
        t.push_ack(); // command frame ACK
        t.push_nack(); // page-list NACK
        {
            let mut bl = Bootloader::new(&mut t);
            let err = bl.extended_erase_pages(&[0, 1, 2], erase_timeout).unwrap_err();
            assert!(matches!(
                err,
                Error::Nack {
                    stage: NackStage::Payload,
                    ..
                }
            ));
        }
        assert_eq!(t.timeouts(), &[erase_timeout, DEFAULT_TIMEOUT_MARKER]);
    }

    #[test]
    fn extended_erase_rejects_bad_page_counts() {
        let mut t = ScriptedTransport::new();
        let mut bl = Bootloader::new(&mut t);
        assert!(matches!(
            bl.extended_erase_pages(&[], Duration::from_secs(1)),
            Err(Error::InvalidArgument { .. })
        ));
        let too_many: Vec<u16> = (0..81).collect();
        assert!(matches!(
            bl.extended_erase_pages(&too_many, Duration::from_secs(1)),
            Err(Error::InvalidArgument { .. })
        ));
    }

    #[test]
    fn write_and_read_block_reject_bad_lengths() {
        let mut t = ScriptedTransport::new();
        let mut bl = Bootloader::new(&mut t);
        assert!(matches!(
            bl.write_block(0, &[]),
            Err(Error::InvalidArgument { .. })
        ));
        assert!(matches!(
            bl.write_block(0, &[0u8; 257]),
            Err(Error::InvalidArgument { .. })
        ));
        assert!(matches!(bl.read_block(0, 0), Err(Error::InvalidArgument { .. })));
        assert!(matches!(
            bl.read_block(0, 257),
            Err(Error::InvalidArgument { .. })
        ));
    }

    #[test]
    fn cmd_frames_match_an3155() {
        assert_eq!(cmd_frame(0x00), [0x00, 0xFF]); // Get
        assert_eq!(cmd_frame(0x02), [0x02, 0xFD]); // Get ID
        assert_eq!(cmd_frame(0x11), [0x11, 0xEE]); // Read Memory
        assert_eq!(cmd_frame(0x21), [0x21, 0xDE]); // Go
        assert_eq!(cmd_frame(0x31), [0x31, 0xCE]); // Write Memory
        assert_eq!(cmd_frame(0x44), [0x44, 0xBB]); // Extended Erase
        assert_eq!(cmd_frame(CMD_READOUT_PROTECT), [0x82, 0x7D]); // Readout Protect
        assert_eq!(cmd_frame(CMD_READOUT_UNPROTECT), [0x92, 0x6D]); // Readout Unprotect
    }

    /// Readout Protect: `82 7D`, first ACK, `accepted` fires, second ACK under the long
    /// timeout, then the default timeout is restored.
    #[test]
    fn readout_protect_handshake_two_acks() {
        let mut t = ScriptedTransport::new();
        t.push_ack(); // command accepted
        t.push_ack(); // option bytes written
        let long = Duration::from_secs(30);
        let mut accepted = 0;
        Bootloader::new(&mut t)
            .readout_protect(long, &mut || accepted += 1)
            .unwrap();
        assert_eq!(accepted, 1);
        assert_eq!(t.written(), &[0x82, 0x7D]);
        assert_eq!(t.timeouts(), &[long, DEFAULT_TIMEOUT_MARKER]);
    }

    /// Readout Unprotect: `92 6D` and the same two-ACK handshake.
    #[test]
    fn readout_unprotect_handshake_two_acks() {
        let mut t = ScriptedTransport::new();
        t.push_ack();
        t.push_ack();
        let long = Duration::from_secs(30);
        Bootloader::new(&mut t)
            .readout_unprotect(long, &mut || {})
            .unwrap();
        assert_eq!(t.written(), &[0x92, 0x6D]);
        assert_eq!(t.timeouts(), &[long, DEFAULT_TIMEOUT_MARKER]);
    }

    /// A NACK to the command frame is a Command-stage NACK, `accepted` never fires and nothing
    /// waits for a second ACK; a NACK in place of the second ACK is the Operation stage, and
    /// the default timeout is still restored.
    #[test]
    fn readout_commands_report_the_nack_stage() {
        let mut t = ScriptedTransport::new();
        t.push_nack();
        let mut accepted = false;
        let err = Bootloader::new(&mut t)
            .readout_protect(Duration::from_secs(30), &mut || accepted = true)
            .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Nack {
                    stage: NackStage::Command,
                    ..
                }
            ),
            "got {err:?}"
        );
        assert!(!accepted);
        assert!(t.timeouts().is_empty(), "no second-ACK wait after a command NACK");

        let mut t = ScriptedTransport::new();
        t.push_ack();
        t.push_nack();
        let long = Duration::from_secs(30);
        let err = Bootloader::new(&mut t)
            .readout_unprotect(long, &mut || {})
            .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Nack {
                    stage: NackStage::Operation,
                    ..
                }
            ),
            "got {err:?}"
        );
        assert_eq!(t.timeouts(), &[long, DEFAULT_TIMEOUT_MARKER]);
    }

    /// Write Memory stays inside main flash: the option bytes (where RDP lives, and RDP = 0xCC
    /// would be the permanent level 2), the data EEPROM and a block straddling the end of
    /// flash are refused before anything is sent.
    #[test]
    fn write_block_refuses_everything_outside_main_flash() {
        let flash_end = target::FLASH_BASE + target::MAX_FLASH_SIZE;
        for (addr, len) in [
            (0x1FF8_0000u32, 8usize), // option bytes (RDP)
            (0x0808_0000, 8),         // data EEPROM
            (0x2000_0000, 8),         // SRAM
            (target::FLASH_BASE - 8, 8),
            (flash_end - 8, 16), // straddles the end of flash
            (flash_end, 8),
        ] {
            let mut t = ScriptedTransport::new();
            let err = Bootloader::new(&mut t)
                .write_block(addr, &vec![0u8; len])
                .unwrap_err();
            assert!(
                matches!(err, Error::InvalidArgument { .. }),
                "0x{addr:08X}: {err:?}"
            );
            assert!(t.written().is_empty(), "0x{addr:08X}: nothing on the wire");
        }
        // The first and the last block of main flash are fine.
        for addr in [target::FLASH_BASE, flash_end - 8] {
            let mut t = ScriptedTransport::new();
            t.push_bytes(&[ACK, ACK, ACK]);
            Bootloader::new(&mut t).write_block(addr, &[0u8; 8]).unwrap();
        }
    }

    /// A stale byte is dropped by `discard_input` (the double-NACK case), a regular reply
    /// is not.
    #[test]
    fn discard_input_drops_only_stale_bytes() {
        let mut t = ScriptedTransport::new();
        t.push_stale(NACK);
        t.push_ack();
        let mut bl = Bootloader::new(&mut t);
        bl.discard_input(Duration::ZERO).unwrap();
        bl.discard_input(Duration::ZERO).unwrap();
        assert_eq!(t.read_byte("after discard").unwrap(), ACK);
    }

    #[test]
    fn address_frame_flash_base() {
        assert_eq!(address_frame(0x0800_0000), [0x08, 0x00, 0x00, 0x00, 0x08]);
        // checksum is the XOR fold of the four address bytes
        let f = address_frame(0x0800_1234);
        assert_eq!(f[4], f[0] ^ f[1] ^ f[2] ^ f[3]);
    }

    #[test]
    fn write_data_frame_checksum() {
        let data = [0xAA, 0xBB, 0xCC, 0xDD];
        let f = write_data_frame(&data);
        let n = (data.len() - 1) as u8; // 3
        assert_eq!(f[0], n);
        assert_eq!(&f[1..5], &data);
        let expected_cs = n ^ 0xAA ^ 0xBB ^ 0xCC ^ 0xDD;
        assert_eq!(*f.last().unwrap(), expected_cs);
    }

    #[test]
    fn parse_chip_id_l0() {
        // STM32L0x3 Get ID payload: 0x04 0x47
        assert_eq!(parse_chip_id(&[0x04, 0x47]), 0x447);
    }

    #[test]
    fn erase_frame_format() {
        // pages [0,1,2]: count N=2 (0x0000,0x0002 hi/lo), then page numbers, then XOR
        let f = erase_frame(&[0, 1, 2]);
        assert_eq!(&f[..f.len() - 1], &[0x00, 0x02, 0, 0, 0, 1, 0, 2]);
        let cs = f[..f.len() - 1].iter().fold(0u8, |a, &b| a ^ b);
        assert_eq!(*f.last().unwrap(), cs);
    }
}
