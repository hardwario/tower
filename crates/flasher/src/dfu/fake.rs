//! A simulated STM32L0 ROM DFU bootloader for the [`super`] engine's tests: the DFU 1.1 state
//! machine as the DfuSe sequences drive it (a command or a data block runs on the first
//! `DFU_GETSTATUS` after its `DFU_DNLOAD`, which answers `dfuDNBUSY`), an address pointer, and a
//! 192 KiB main flash that erases to zeros (STM32L0) and refuses to program a non-zero location
//! (`errPROG`, the L0's NOTZEROERR). Readout protection and a few faults can be switched on.

use std::time::Duration;

use super::{
    CMD_ERASE, CMD_SET_ADDRESS, DFU_ABORT, DFU_CLRSTATUS, DFU_DNLOAD, DFU_GETSTATUS, DFU_UPLOAD,
    STATE_DFU_IDLE, STATE_DNBUSY, STATE_DNLOAD_IDLE, STATE_ERROR, STATE_MANIFEST, STATUS_ERR_VENDOR,
    STATUS_OK, Transport, request_name,
};
use crate::error::{Error, Result};

const BASE: u32 = 0x0800_0000;
const SIZE: usize = 192 * 1024;
const PAGE: u32 = 128;
const STATE_DNLOAD_SYNC: u8 = 3;
const STATE_MANIFEST_SYNC: u8 = 6;
const STATE_UPLOAD_IDLE: u8 = 9;
const ERR_PROG: u8 = 0x06;
const ERR_ADDRESS: u8 = 0x08;
const ERR_STALLEDPKT: u8 = 0x0F;

/// A memory operation the device carried out, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Op {
    Erase(u32),
    Write(u32, usize),
    Read(u32, usize),
}

enum Pending {
    Command(Vec<u8>),
    Write(Vec<u8>),
}

pub(super) struct FakeDfu {
    pub state: u8,
    pub status: u8,
    pub mem: Vec<u8>,
    /// RDP level 1: memory reads, writes and erases are refused with `errVENDOR`.
    pub protected: bool,
    /// Refuse every `DFU_UPLOAD` with this status (a failure that is not protection).
    pub upload_status: Option<u8>,
    /// Flip the byte at this address when a write covers it (a silent programming fault).
    pub corrupt_write_at: Option<u32>,
    /// Extra `dfuDNBUSY` answers per operation.
    pub busy_polls: u32,
    pub ops: Vec<Op>,
    /// Every `DFU_DNLOAD` block-0 payload, as sent.
    pub commands: Vec<Vec<u8>>,
    /// Where the zero-length `DFU_DNLOAD` jumped (the address pointer), once DFU was left.
    pub left: Option<u32>,
    pub sleeps: u32,
    pointer: u32,
    pending: Option<Pending>,
    result: u8,
    busy_left: u32,
}

impl FakeDfu {
    pub fn new() -> Self {
        Self {
            state: STATE_DFU_IDLE,
            status: STATUS_OK,
            mem: vec![0; SIZE],
            protected: false,
            upload_status: None,
            corrupt_write_at: None,
            busy_polls: 0,
            ops: Vec::new(),
            commands: Vec::new(),
            left: None,
            sleeps: 0,
            pointer: 0,
            pending: None,
            result: STATUS_OK,
            busy_left: 0,
        }
    }

    fn stall(&mut self, request: u8, status: u8) -> Error {
        self.state = STATE_ERROR;
        self.status = status;
        Error::UsbStall {
            request: request_name(request),
        }
    }

    fn offset(addr: u32, len: usize) -> Option<usize> {
        let off = addr.checked_sub(BASE)? as usize;
        (off + len <= SIZE).then_some(off)
    }

    /// Carry out the pending operation; its DFU status.
    fn execute(&mut self) -> u8 {
        match self.pending.take() {
            Some(Pending::Command(c)) => match c.as_slice() {
                [CMD_SET_ADDRESS, a @ ..] if a.len() == 4 => {
                    self.pointer = u32::from_le_bytes([a[0], a[1], a[2], a[3]]);
                    STATUS_OK
                }
                [CMD_ERASE, a @ ..] if a.len() == 4 => {
                    let addr = u32::from_le_bytes([a[0], a[1], a[2], a[3]]);
                    if self.protected {
                        return STATUS_ERR_VENDOR;
                    }
                    let Some(off) = Self::offset(addr, PAGE as usize).filter(|_| addr % PAGE == 0) else {
                        return ERR_ADDRESS;
                    };
                    self.mem[off..off + PAGE as usize].fill(0);
                    self.ops.push(Op::Erase(addr));
                    STATUS_OK
                }
                other => panic!("the engine sent a DfuSe command it must never send: {other:02x?}"),
            },
            Some(Pending::Write(data)) => {
                if self.protected {
                    return STATUS_ERR_VENDOR;
                }
                let addr = self.pointer;
                let Some(off) = Self::offset(addr, data.len()) else {
                    return ERR_ADDRESS;
                };
                if self.mem[off..off + data.len()].iter().any(|&b| b != 0) {
                    return ERR_PROG;
                }
                self.mem[off..off + data.len()].copy_from_slice(&data);
                if let Some(bad) = self.corrupt_write_at
                    && let Some(i) = bad
                        .checked_sub(addr)
                        .map(|i| i as usize)
                        .filter(|&i| i < data.len())
                {
                    self.mem[off + i] ^= 0xFF;
                }
                self.ops.push(Op::Write(addr, data.len()));
                STATUS_OK
            }
            None => STATUS_OK,
        }
    }

    fn status_bytes(&self) -> Vec<u8> {
        // bwPollTimeout 1 ms.
        vec![self.status, 1, 0, 0, self.state, 0]
    }
}

impl Transport for FakeDfu {
    fn control_out(&mut self, request: u8, value: u16, data: &[u8]) -> Result<()> {
        match request {
            DFU_DNLOAD => {
                if self.state != STATE_DFU_IDLE && self.state != STATE_DNLOAD_IDLE {
                    return Err(self.stall(request, ERR_STALLEDPKT));
                }
                match (value, data.is_empty()) {
                    (0, false) => {
                        self.commands.push(data.to_vec());
                        self.pending = Some(Pending::Command(data.to_vec()));
                        self.state = STATE_DNLOAD_SYNC;
                    }
                    (2, false) => {
                        self.pending = Some(Pending::Write(data.to_vec()));
                        self.state = STATE_DNLOAD_SYNC;
                    }
                    (_, true) if value >= 2 => self.state = STATE_MANIFEST_SYNC,
                    _ => return Err(self.stall(request, ERR_STALLEDPKT)),
                }
                Ok(())
            }
            DFU_CLRSTATUS if self.state == STATE_ERROR => {
                self.state = STATE_DFU_IDLE;
                self.status = STATUS_OK;
                Ok(())
            }
            DFU_ABORT if self.state != STATE_ERROR => {
                self.state = STATE_DFU_IDLE;
                Ok(())
            }
            _ => Err(self.stall(request, ERR_STALLEDPKT)),
        }
    }

    fn control_in(&mut self, request: u8, value: u16, length: u16) -> Result<Vec<u8>> {
        match request {
            DFU_GETSTATUS => {
                match self.state {
                    STATE_DNLOAD_SYNC => {
                        self.result = self.execute();
                        self.busy_left = self.busy_polls;
                        self.state = STATE_DNBUSY;
                    }
                    STATE_DNBUSY if self.busy_left > 0 => self.busy_left -= 1,
                    STATE_DNBUSY => {
                        if self.result == STATUS_OK {
                            self.state = STATE_DNLOAD_IDLE;
                        } else {
                            self.state = STATE_ERROR;
                            self.status = self.result;
                        }
                    }
                    STATE_MANIFEST_SYNC => {
                        self.left = Some(self.pointer);
                        self.state = STATE_MANIFEST;
                    }
                    // Jumped to the application: off the bus.
                    STATE_MANIFEST => {
                        return Err(Error::Usb {
                            context: "DFU_GETSTATUS",
                            source: std::io::Error::from(std::io::ErrorKind::NotConnected),
                        });
                    }
                    _ => {}
                }
                Ok(self.status_bytes())
            }
            DFU_UPLOAD => {
                if self.state != STATE_DFU_IDLE && self.state != STATE_UPLOAD_IDLE {
                    return Err(self.stall(request, ERR_STALLEDPKT));
                }
                if self.protected {
                    return Err(self.stall(request, STATUS_ERR_VENDOR));
                }
                if let Some(status) = self.upload_status {
                    return Err(self.stall(request, status));
                }
                assert_eq!(
                    value, 2,
                    "the engine uploads block 2 right after Set Address Pointer"
                );
                let len = usize::from(length);
                let Some(off) = Self::offset(self.pointer, len) else {
                    return Err(self.stall(request, ERR_ADDRESS));
                };
                self.ops.push(Op::Read(self.pointer, len));
                self.state = STATE_UPLOAD_IDLE;
                Ok(self.mem[off..off + len].to_vec())
            }
            _ => Err(self.stall(request, ERR_STALLEDPKT)),
        }
    }

    fn sleep(&mut self, _duration: Duration) {
        self.sleeps += 1;
    }
}
