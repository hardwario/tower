//! A simulated STM32L0 behind its ROM UART bootloader (AN3155), for the tests only (the module
//! is `#[cfg(test)]` in `lib.rs`).
//!
//! Unlike the scripted transports, it keeps state: a main-flash array that erases to 0x00 (the
//! STM32L0's erased value, unlike most STM32 families) and refuses to program a byte that is not
//! erased, so a write that skipped its erase fails; the readout-protection level (at level 1 only
//! Get, Get Version, Get ID and Readout Unprotect are served); and the NRST/BOOT0 lines of the
//! TOWER auto-reset circuit. It parses the host's bytes as a stream, checks every frame (command
//! complements, address and data checksums, the main-flash bounds, 8-byte write alignment, the
//! density boundary of Extended Erase) and logs every command it carried out as an [`Op`], so a
//! test pins what reached the wire, at which address, in which order.
//!
//! One fault can be injected ([`FakeStm32::with_fault`]); once it fired, every byte the host
//! still sends is counted ([`FakeStm32::bytes_after_fault`]), so a test can assert that nothing
//! followed a failure.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use crate::bootloader::Transport;
use crate::error::{Error, Result};
use crate::target;

const ACK: u8 = 0x79;
const NACK: u8 = 0x1F;

/// The STM32L0's erased flash value.
pub(crate) const ERASED: u8 = 0x00;

/// A command the simulated bootloader carried out (or was failing by an injected fault).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    Init,
    GetId,
    Get,
    Read { addr: u32, len: usize },
    Write { addr: u32, len: usize },
    Erase(Vec<u16>),
    Go(u32),
    ReadoutProtect,
    ReadoutUnprotect,
}

/// What an injected fault sends in place of the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    /// A NACK.
    Nack,
    /// Nothing: the host's read times out.
    Silent,
    /// A stray 0x00 byte.
    Garbage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Running the application, which ignores bootloader bytes.
    App,
    /// NRST asserted, or released and still ramping up: BOOT0 not latched yet.
    Reset,
    /// The ROM bootloader; `init` once the 0x7F auto-baud byte was ACKed.
    Bootloader { init: bool },
}

/// The frame the bootloader expects next.
#[derive(Debug, Clone, Copy)]
enum Stage {
    Command,
    ReadAddress,
    ReadLength(u32),
    WriteAddress,
    WriteData(u32),
    ErasePages,
    GoAddress,
}

pub(crate) struct FakeStm32 {
    id: u16,
    /// Main flash, from `FLASH_BASE`.
    pub(crate) flash: Vec<u8>,
    pub(crate) protected: bool,
    pub(crate) ops: Vec<Op>,
    pub(crate) bytes_after_fault: usize,
    mode: Mode,
    stage: Stage,
    frame: Vec<u8>,
    out: VecDeque<u8>,
    rts: bool,
    dtr: bool,
    /// `(opcode, occurrence, fault)`: the fault replaces the reply at that command's fault point
    /// (Read Memory: the length; Write Memory: the data; Readout Protect / Unprotect: the second
    /// ACK), on its `occurrence`-th time there (0-based).
    fault: Option<(u8, usize, Fault)>,
    fault_points: HashMap<u8, usize>,
    fault_fired: bool,
}

impl FakeStm32 {
    /// A chip with product id `id` and `flash_kib` KiB of erased main flash, not protected,
    /// running its application with the lines in the run state (T,T).
    pub(crate) fn new(id: u16, flash_kib: usize) -> Self {
        FakeStm32 {
            id,
            flash: vec![ERASED; flash_kib * 1024],
            protected: false,
            ops: Vec::new(),
            bytes_after_fault: 0,
            mode: Mode::App,
            stage: Stage::Command,
            frame: Vec::new(),
            out: VecDeque::new(),
            rts: true,
            dtr: true,
            fault: None,
            fault_points: HashMap::new(),
            fault_fired: false,
        }
    }

    /// Already in a bootloader session (0x7F ACKed), for a `Bootloader` driven on the chip
    /// directly, with no reset lines.
    pub(crate) fn in_session(mut self) -> Self {
        self.mode = Mode::Bootloader { init: true };
        self
    }

    /// Readout protection (RDP level 1) active.
    pub(crate) fn protected(mut self) -> Self {
        self.protected = true;
        self
    }

    /// Inject `fault` at the `occurrence`-th (0-based) fault point of command `opcode`.
    pub(crate) fn with_fault(mut self, opcode: u8, occurrence: usize, fault: Fault) -> Self {
        self.fault = Some((opcode, occurrence, fault));
        self
    }

    pub(crate) fn running_app(&self) -> bool {
        self.mode == Mode::App
    }

    pub(crate) fn in_bootloader(&self) -> bool {
        matches!(self.mode, Mode::Bootloader { .. })
    }

    /// A modem-line write (`'R'` = RTS, `'D'` = DTR). The TOWER circuit asserts NRST while RTS
    /// is asserted and DTR is not, and drives BOOT0 high while DTR is asserted and RTS is not.
    /// After a reset, the cap keeps NRST ramping through (T,T); the next other state is where
    /// the chip comes up: (F,T) latches BOOT0 high (the bootloader), (F,F) boots the app.
    pub(crate) fn set_line(&mut self, which: char, level: bool) {
        match which {
            'R' => self.rts = level,
            _ => self.dtr = level,
        }
        if self.rts && !self.dtr {
            self.mode = Mode::Reset;
            self.stage = Stage::Command;
            self.frame.clear();
        } else if self.mode == Mode::Reset && !(self.rts && self.dtr) {
            self.mode = if self.dtr {
                Mode::Bootloader { init: false }
            } else {
                Mode::App
            };
        }
    }

    /// Bytes from the host.
    pub(crate) fn receive(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.fault_fired {
                self.bytes_after_fault += 1;
            }
            self.byte(b);
        }
    }

    /// The next byte the chip sent, if any.
    pub(crate) fn reply(&mut self) -> Option<u8> {
        self.out.pop_front()
    }

    /// The host dropped its receive buffer.
    pub(crate) fn clear_replies(&mut self) {
        self.out.clear();
    }

    fn byte(&mut self, b: u8) {
        match self.mode {
            Mode::App | Mode::Reset => {}
            Mode::Bootloader { init: false } => {
                if b == 0x7F {
                    self.mode = Mode::Bootloader { init: true };
                    self.ops.push(Op::Init);
                    self.out.push_back(ACK);
                }
            }
            Mode::Bootloader { init: true } => {
                self.frame.push(b);
                if self.frame.len() == self.frame_len() {
                    let frame = std::mem::take(&mut self.frame);
                    let stage = std::mem::replace(&mut self.stage, Stage::Command);
                    self.serve(stage, &frame);
                }
            }
        }
    }

    /// The length of the frame being received (known once its length bytes are in).
    fn frame_len(&self) -> usize {
        let f = &self.frame;
        match self.stage {
            Stage::Command | Stage::ReadLength(_) => 2,
            Stage::ReadAddress | Stage::WriteAddress | Stage::GoAddress => 5,
            // N, then N + 1 data bytes, then the checksum.
            Stage::WriteData(_) => usize::from(f[0]) + 3,
            // N (2 bytes), then N + 1 page numbers (2 bytes each), then the checksum.
            Stage::ErasePages if f.len() < 2 => 2,
            Stage::ErasePages => (usize::from(u16::from_be_bytes([f[0], f[1]])) + 1) * 2 + 3,
        }
    }

    fn ack(&mut self) {
        self.out.push_back(ACK);
    }

    fn nack(&mut self) {
        self.out.push_back(NACK);
    }

    /// The address of an address frame, or `None` when its checksum is wrong.
    fn address(f: &[u8]) -> Option<u32> {
        (f[4] == f[0] ^ f[1] ^ f[2] ^ f[3]).then(|| u32::from_be_bytes([f[0], f[1], f[2], f[3]]))
    }

    /// The offset into `flash` of `len` bytes at `addr`, if they all lie in main flash.
    fn offset(&self, addr: u32, len: usize) -> Option<usize> {
        let start = addr.checked_sub(target::FLASH_BASE)? as usize;
        (start + len <= self.flash.len()).then_some(start)
    }

    /// Count one more pass through `opcode`'s fault point; on the faulty one, send the fault and
    /// return `true` (the normal reply is skipped).
    fn inject(&mut self, opcode: u8) -> bool {
        match self.take_fault(opcode) {
            None => false,
            Some(fault) => {
                match fault {
                    Fault::Nack => self.nack(),
                    Fault::Silent => {}
                    Fault::Garbage => self.out.push_back(0x00),
                }
                true
            }
        }
    }

    fn take_fault(&mut self, opcode: u8) -> Option<Fault> {
        let seen = self.fault_points.entry(opcode).or_default();
        let occurrence = *seen;
        *seen += 1;
        match self.fault {
            Some((op, at, fault)) if op == opcode && at == occurrence => {
                self.fault = None;
                self.fault_fired = true;
                Some(fault)
            }
            _ => None,
        }
    }

    fn serve(&mut self, stage: Stage, f: &[u8]) {
        match stage {
            Stage::Command => self.command(f[0], f[1]),
            Stage::ReadAddress => match Self::address(f) {
                Some(addr) if self.offset(addr, 1).is_some() => {
                    self.ack();
                    self.stage = Stage::ReadLength(addr);
                }
                _ => self.nack(),
            },
            Stage::ReadLength(addr) => {
                let len = usize::from(f[0]) + 1;
                let Some(start) = self.offset(addr, len).filter(|_| f[1] == !f[0]) else {
                    return self.nack();
                };
                self.ops.push(Op::Read { addr, len });
                if self.inject(0x11) {
                    return;
                }
                self.ack();
                let data = self.flash[start..start + len].to_vec();
                self.out.extend(data);
            }
            Stage::WriteAddress => match Self::address(f) {
                Some(addr)
                    if self.offset(addr, 1).is_some()
                        && (addr as usize).is_multiple_of(target::WRITE_ALIGN) =>
                {
                    self.ack();
                    self.stage = Stage::WriteData(addr);
                }
                _ => self.nack(),
            },
            Stage::WriteData(addr) => {
                let len = usize::from(f[0]) + 1;
                let data = &f[1..=len];
                let checksum = data.iter().fold(f[0], |cs, &b| cs ^ b);
                let target = self.offset(addr, len).filter(|&start| {
                    checksum == f[len + 1]
                        && len.is_multiple_of(target::WRITE_ALIGN)
                        && self.flash[start..start + len].iter().all(|&b| b == ERASED)
                });
                let Some(start) = target else {
                    return self.nack();
                };
                self.ops.push(Op::Write { addr, len });
                if self.inject(0x31) {
                    return;
                }
                self.flash[start..start + len].copy_from_slice(data);
                self.ack();
            }
            Stage::ErasePages => {
                let (list, checksum) = f.split_at(f.len() - 1);
                let pages: Vec<u16> = list[2..]
                    .chunks(2)
                    .map(|p| u16::from_be_bytes([p[0], p[1]]))
                    .collect();
                let density = self.flash.len() / target::PAGE_SIZE;
                // AN3155: a page out of range fails the whole command, nothing is erased.
                if list.iter().fold(0, |cs, &b| cs ^ b) != checksum[0]
                    || pages.iter().any(|&p| usize::from(p) >= density)
                {
                    return self.nack();
                }
                for &p in &pages {
                    let start = usize::from(p) * target::PAGE_SIZE;
                    self.flash[start..start + target::PAGE_SIZE].fill(ERASED);
                }
                self.ops.push(Op::Erase(pages));
                self.ack();
            }
            Stage::GoAddress => match Self::address(f) {
                Some(addr) if self.offset(addr, 1).is_some() => {
                    self.ops.push(Op::Go(addr));
                    self.ack();
                    self.mode = Mode::App;
                }
                _ => self.nack(),
            },
        }
    }

    fn command(&mut self, opcode: u8, complement: u8) {
        if complement != !opcode {
            return self.nack();
        }
        // RDP level 1: only Get, Get Version, Get ID and Readout Unprotect are served (AN3155).
        if self.protected && !matches!(opcode, 0x00 | 0x01 | 0x02 | 0x92) {
            return self.nack();
        }
        match opcode {
            0x00 => {
                self.ops.push(Op::Get);
                // Version 3.1, then the commands this simulation serves.
                let commands = [0x00, 0x02, 0x11, 0x21, 0x31, 0x44, 0x82, 0x92];
                self.out.extend([ACK, commands.len() as u8, 0x31]);
                self.out.extend(commands);
                self.ack();
            }
            0x02 => {
                self.ops.push(Op::GetId);
                let [hi, lo] = self.id.to_be_bytes();
                self.out.extend([ACK, 0x01, hi, lo, ACK]);
            }
            0x11 | 0x21 | 0x31 | 0x44 => {
                self.ack();
                self.stage = match opcode {
                    0x11 => Stage::ReadAddress,
                    0x21 => Stage::GoAddress,
                    0x31 => Stage::WriteAddress,
                    _ => Stage::ErasePages,
                };
            }
            0x82 | 0x92 => self.change_protection(opcode),
            _ => self.nack(),
        }
    }

    /// Readout Protect (0x82) / Unprotect (0x92): the first ACK, the change, the second ACK (or
    /// the injected fault), then the self-reset that loads the option bytes; BOOT0 is low by
    /// then, so the application boots.
    fn change_protection(&mut self, opcode: u8) {
        let protect = opcode == 0x82;
        self.ack();
        self.ops.push(if protect {
            Op::ReadoutProtect
        } else {
            Op::ReadoutUnprotect
        });
        let fault = self.take_fault(opcode);
        if fault == Some(Fault::Nack) {
            // AN3155: the update failed, the level stays.
            return self.nack();
        }
        if !protect {
            // RM0367 3.4.1: leaving level 1 mass-erases the flash (and the data EEPROM).
            self.flash.fill(ERASED);
        }
        self.protected = protect;
        match fault {
            None => self.ack(),
            Some(Fault::Garbage) => self.out.push_back(0x00),
            Some(_) => {}
        }
        self.mode = Mode::App;
    }
}

/// Drive a `Bootloader` on the chip directly (no port, no reset lines; see
/// [`FakeStm32::in_session`]). A read with no reply pending times out.
impl Transport for FakeStm32 {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.receive(buf);
        Ok(())
    }

    fn read_exact_buf(&mut self, buf: &mut [u8], context: &str) -> Result<()> {
        for slot in buf.iter_mut() {
            *slot = self.reply().ok_or_else(|| Error::Timeout {
                context: context.to_string(),
            })?;
        }
        Ok(())
    }

    fn clear_input(&mut self) -> Result<()> {
        self.clear_replies();
        Ok(())
    }

    fn set_timeout(&mut self, _: Duration) -> Result<()> {
        Ok(())
    }

    fn reset_timeout(&mut self) -> Result<()> {
        Ok(())
    }
}
