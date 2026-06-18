//! STM32 USART bootloader protocol engine (AN3155).

use std::time::Duration;

use crate::error::{Error, Result};
use crate::port::Port;

const ACK: u8 = 0x79;
const NACK: u8 = 0x1F;
const INIT: u8 = 0x7F;

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

pub struct Bootloader<'a> {
    port: &'a mut Port,
}

impl<'a> Bootloader<'a> {
    pub fn new(port: &'a mut Port) -> Self {
        Bootloader { port }
    }

    fn read_ack(&mut self, context: &str) -> Result<()> {
        match self.port.read_byte(context)? {
            ACK => Ok(()),
            NACK => Err(Error::Nack {
                context: context.to_string(),
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
        self.read_ack(context)
    }

    fn send_address(&mut self, addr: u32, context: &str) -> Result<()> {
        self.port.write_all(&address_frame(addr))?;
        self.read_ack(context)
    }

    /// Auto-baud init: send a single `0x7F` and expect an ACK. Called after a
    /// fresh reset into the bootloader, so a NACK ("already initialised") means
    /// something is off — `connect` then retries with another reset.
    pub fn init(&mut self) -> Result<()> {
        self.port.clear_input()?;
        self.port.write_all(&[INIT])?;
        self.read_ack("bootloader init (0x7F)")
    }

    /// `Get ID` -> 16-bit product id (0x447 for STM32L0x3).
    pub fn get_id(&mut self) -> Result<u16> {
        self.send_command(CMD_GET_ID, "Get ID command")?;
        let n = self.port.read_byte("Get ID length")?;
        let mut buf = vec![0u8; n as usize + 1];
        self.port.read_exact_buf(&mut buf, "Get ID payload")?;
        self.read_ack("Get ID final ACK")?;
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
        self.read_ack("Get final ACK")?;
        Ok((version, cmds))
    }

    /// Extended Erase of an explicit page list (max 80 per call). `timeout`
    /// covers the (slow) erase ACK.
    pub fn extended_erase_pages(&mut self, pages: &[u16], timeout: Duration) -> Result<()> {
        debug_assert!(!pages.is_empty() && pages.len() <= 80);
        self.send_command(CMD_EXT_ERASE, "Extended Erase command")?;
        self.port.set_timeout(timeout)?;
        let result = self
            .port
            .write_all(&erase_frame(pages))
            .and_then(|()| self.read_ack("Extended Erase pages"));
        let _ = self.port.reset_timeout();
        result
    }

    /// Write up to 256 bytes at `addr`. Data should already be aligned/padded.
    pub fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        debug_assert!(!data.is_empty() && data.len() <= 256);
        self.send_command(CMD_WRITE_MEMORY, "Write Memory command")?;
        self.send_address(addr, "Write Memory address")?;
        self.port.write_all(&write_data_frame(data))?;
        self.read_ack("Write Memory data")
    }

    /// Read `len` (1..=256) bytes from `addr`.
    pub fn read_block(&mut self, addr: u32, len: usize) -> Result<Vec<u8>> {
        debug_assert!((1..=256).contains(&len));
        self.send_command(CMD_READ_MEMORY, "Read Memory command")?;
        self.send_address(addr, "Read Memory address")?;
        let n = (len - 1) as u8;
        self.port.write_all(&[n, n ^ 0xFF])?;
        self.read_ack("Read Memory length")?;
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
mod tests {
    use super::*;

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
