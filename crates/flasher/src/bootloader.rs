//! STM32 USART bootloader protocol engine (AN3155).

use std::time::Duration;

use crate::error::{Error, NackStage, Result};

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
    /// something is off — `connect` then retries with another reset.
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
        self.port
            .read_exact_buf(&mut cmds, "Get supported commands")?;
        self.read_ack("Get final ACK", NackStage::Payload)?;
        Ok((version, cmds))
    }

    /// Extended Erase of an explicit page list (1..=80 pages per call).
    /// `timeout` covers the (slow) erase ACK.
    ///
    /// The command byte + complement is acknowledged first (a NACK here is
    /// [`NackStage::Command`] — usually line corruption); the page-list payload
    /// is acknowledged second (a NACK here is [`NackStage::Payload`] — the
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
    pub fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        if data.is_empty() || data.len() > 256 {
            return Err(Error::InvalidArgument {
                context: "write_block expects 1..=256 bytes",
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
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// In-memory [`Transport`] for protocol tests — no hardware.
    ///
    /// Two ways to drive it, which compose:
    ///   * a **manual reply queue** (`push_ack`, `push_nack`, `push_bytes`, …):
    ///     reads pop these bytes in order — for deterministic scripts;
    ///   * an optional **erase-boundary simulation** (`set_erase_boundary`):
    ///     when set, Extended Erase (0x44) command+page-list frames are answered
    ///     automatically — the command byte ACKs, and the page list ACKs iff
    ///     every page is `< boundary` (erased pages are recorded), else NACKs.
    ///     This models a part of unknown density and lets [`erase_chip`]'s
    ///     bisection run against a moving boundary.
    ///
    /// All written bytes are recorded (`take_written`).
    pub struct ScriptedTransport {
        written: Vec<u8>,
        replies: VecDeque<u8>,
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
            self.replies.push_back(ACK);
        }

        pub fn push_nack(&mut self) {
            self.replies.push_back(NACK);
        }

        pub fn push_byte(&mut self, b: u8) {
            self.replies.push_back(b);
        }

        pub fn push_bytes(&mut self, bytes: &[u8]) {
            self.replies.extend(bytes.iter().copied());
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
                    self.replies.push_back(ACK); // command byte accepted
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
                        self.replies.push_back(ACK);
                    } else {
                        self.replies.push_back(NACK);
                    }
                    return Ok(());
                }
            }
            Ok(())
        }

        fn read_exact_buf(&mut self, buf: &mut [u8], context: &str) -> Result<()> {
            for slot in buf.iter_mut() {
                *slot = self.replies.pop_front().ok_or_else(|| Error::Timeout {
                    context: context.to_string(),
                })?;
            }
            Ok(())
        }

        fn clear_input(&mut self) -> Result<()> {
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
            let err = bl
                .extended_erase_pages(&[0, 1, 2], erase_timeout)
                .unwrap_err();
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
        assert!(matches!(
            bl.read_block(0, 0),
            Err(Error::InvalidArgument { .. })
        ));
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
