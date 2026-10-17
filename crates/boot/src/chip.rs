//! Read-only chip identification over the ROM bootloader: the product id
//! (`Get ID`), its STM32L0 family name, and the bootloader's protocol version and
//! command set (`Get`). This is what `tower chip` prints (formerly `jolt info`).

use crate::bootloader::{Bootloader, Transport};
use crate::error::Result;
use crate::flash::{self, Progress};
use crate::port::Port;
use crate::target;

/// What the STM32 ROM bootloader reports about the attached part; read with [`info`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipInfo {
    /// STM32 product id from `Get ID` (e.g. `0x447` for the STM32L07x/L08x). This is the
    /// product id sent over UART, unrelated to any USB-bridge PID.
    pub id: u16,
    /// Family name for [`id`](Self::id) ([`target::chip_name`]); `"unknown"` when the id is not
    /// a recognized STM32L0 product id (see [`is_known`](Self::is_known)).
    pub family: &'static str,
    /// Bootloader protocol version byte from `Get`, one version digit per nibble (`0x31` is
    /// v3.1), or `None` when `Get` failed (the id is the essential part; see [`info`]).
    pub bootloader_version: Option<u8>,
    /// The command bytes the bootloader advertises via `Get` (AN3155 opcodes, e.g. `0x31` Write
    /// Memory); empty when `Get` failed.
    pub commands: Vec<u8>,
}

impl ChipInfo {
    /// Whether [`id`](Self::id) is a recognized STM32L0-family product id.
    pub fn is_known(&self) -> bool {
        target::is_known(self.id)
    }

    /// The bootloader version as `(major, minor)` (the version byte's two nibbles), if `Get`
    /// answered.
    pub fn bootloader_version_parts(&self) -> Option<(u8, u8)> {
        self.bootloader_version.map(|v| (v >> 4, v & 0x0F))
    }
}

/// Enter the bootloader, read the chip id (`Get ID`) and the bootloader's version and command
/// set (`Get`), then reset back into the application. Read-only: nothing is erased or written,
/// and the main-flash application is left running.
///
/// The application is restored on every path, so even a failed read-only query never leaves
/// it stopped: a failed bootloader entry is restored by [`flash::connect`]; a failed `Get ID`
/// resets into the application before its error is returned; a successful read ends with
/// [`Port::reset_into_app`], whose error is returned (the application may then not be
/// running). A failed `Get` is not an error: the id is the essential part, so the result
/// carries `bootloader_version: None` and an empty command list instead.
///
/// Emits [`Progress::Connecting`] / [`Progress::ConnectError`] during bootloader entry and
/// [`Progress::ChipIdentified`] once the id is read, like [`flash::flash`] and
/// [`flash::erase`].
pub fn info(port: &mut Port, progress: &mut dyn FnMut(Progress)) -> Result<ChipInfo> {
    flash::connect(port, progress)?;
    let info = match read_info(&mut Bootloader::new(port), progress) {
        Ok(info) => info,
        Err(e) => {
            // Get ID failed before anything else happened: the app is intact, put it back.
            let _ = port.reset_into_app();
            return Err(e);
        }
    };
    port.reset_into_app()?;
    Ok(info)
}

/// The bootloader-protocol core of [`info`]: `Get ID` (an error propagates) then `Get` (a
/// failure only empties the optional fields). Generic over the transport so it can run against
/// a scripted device in tests; does not touch the reset lines.
fn read_info<T: Transport>(bl: &mut Bootloader<T>, progress: &mut dyn FnMut(Progress)) -> Result<ChipInfo> {
    let id = bl.get_id()?;
    progress(Progress::ChipIdentified { id });
    let (bootloader_version, commands) = match bl.get() {
        Ok((version, commands)) => (Some(version), commands),
        Err(_) => (None, Vec::new()),
    };
    Ok(ChipInfo {
        id,
        family: target::chip_name(id),
        bootloader_version,
        commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::flash::no_progress;
    use crate::port::tests::{FakePort, FakeState};
    use std::sync::{Arc, Mutex};

    const ACK: u8 = 0x79;

    /// The modem-line tail of `Port::reset_into_app`: RESET asserted with BOOT0 low, then
    /// released (the same pin the flash-engine tests use).
    const APP_RESET_TAIL: [(char, bool); 3] = [('R', true), ('D', false), ('R', false)];

    /// A `Port` over the in-memory `FakePort` with `replies` scripted as the device's answers
    /// (anything past them reads as 0x00, i.e. a protocol error).
    fn fake_port(replies: &[u8]) -> (Port, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState {
            timeout: crate::port::DEFAULT_TIMEOUT,
            replies: replies.iter().copied().collect(),
            ..Default::default()
        }));
        (Port::from_handle(Box::new(FakePort::new(state.clone()))), state)
    }

    /// Init ACK, then `Get ID` (cmd ACK, N=1, 0x04 0x47, final ACK) for an STM32L083CZ.
    const INIT_AND_ID_447: [u8; 6] = [ACK, ACK, 0x01, 0x04, 0x47, ACK];

    /// The full read: id, family, version, commands; only read commands go on the wire, and the
    /// device ends up reset into its application.
    #[test]
    fn info_reads_id_version_and_commands_then_restores_the_app() {
        let mut replies = INIT_AND_ID_447.to_vec();
        // Get: cmd ACK, N=3 command bytes, version 0x31, the commands, final ACK.
        replies.extend_from_slice(&[ACK, 0x03, 0x31, 0x00, 0x02, 0x31, ACK]);
        let (mut port, state) = fake_port(&replies);
        let mut seen = Vec::new();
        let info = info(&mut port, &mut |p| seen.push(p)).unwrap();

        assert_eq!(info.id, 0x447);
        assert_eq!(info.family, target::chip_name(0x447));
        assert!(info.is_known());
        assert_eq!(info.bootloader_version, Some(0x31));
        assert_eq!(info.bootloader_version_parts(), Some((3, 1)));
        assert_eq!(info.commands, vec![0x00, 0x02, 0x31]);
        assert!(
            seen.iter()
                .any(|p| matches!(p, Progress::ChipIdentified { id: 0x447 })),
            "ChipIdentified reported: {seen:?}"
        );

        let st = state.lock().unwrap();
        // 0x7F init, Get ID (02 FD), Get (00 FF): nothing that erases or writes.
        assert_eq!(st.written, vec![0x7F, 0x02, 0xFD, 0x00, 0xFF]);
        assert!(
            st.lines.ends_with(&APP_RESET_TAIL),
            "reset into app: {:?}",
            st.lines
        );
    }

    /// A failed `Get` keeps the id (the essential part) and still restores the application.
    #[test]
    fn info_without_get_answer_keeps_the_id() {
        // Nothing scripted after Get ID: the Get command's ACK reads 0x00 (unexpected byte).
        let (mut port, state) = fake_port(&INIT_AND_ID_447);
        let info = info(&mut port, &mut no_progress).unwrap();
        assert_eq!(info.id, 0x447);
        assert_eq!(info.bootloader_version, None);
        assert_eq!(info.bootloader_version_parts(), None);
        assert!(info.commands.is_empty());
        assert!(state.lock().unwrap().lines.ends_with(&APP_RESET_TAIL));
    }

    /// An unrecognized product id is reported as such, not rejected (read-only query).
    #[test]
    fn info_reports_an_unknown_id() {
        let (mut port, _state) = fake_port(&[ACK, ACK, 0x01, 0x04, 0x99, ACK]);
        let info = info(&mut port, &mut no_progress).unwrap();
        assert_eq!(info.id, 0x499);
        assert_eq!(info.family, "unknown");
        assert!(!info.is_known());
    }

    /// A failed `Get ID` is an error, and the device is reset back into its application first.
    #[test]
    fn info_get_id_failure_restores_the_app() {
        // Init ACK, then the Get ID command is NACKed.
        let (mut port, state) = fake_port(&[ACK, 0x1F]);
        let err = info(&mut port, &mut no_progress).unwrap_err();
        assert!(matches!(err, Error::Nack { .. }), "got {err:?}");
        assert!(state.lock().unwrap().lines.ends_with(&APP_RESET_TAIL));
    }
}
