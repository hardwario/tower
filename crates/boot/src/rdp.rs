//! Readout protection (RDP) of the STM32L0 flash: the opt-in production lock.
//!
//! TOWER dev boards stay open (RDP level 0). For a production unit, [`protect`] selects RDP
//! level 1, so the firmware and the data EEPROM (radio keys, pairing, settings, TX counters)
//! can no longer be read out through the ROM bootloader or SWD; [`unprotect`] goes back to
//! level 0 at the price of a mass erase. These two functions are the only places the Readout
//! Protect (0x82) and Readout Unprotect (0x92) commands are sent (a test below pins that), and
//! nothing in this crate writes the option bytes: Write Memory is bounded to main flash
//! ([`Bootloader::write_block`]). Level 2, which is permanent, therefore cannot be reached from
//! this tooling.
//!
//! # The facts this relies on
//!
//! STM32L0 reference manual (RM0367 Rev 8, 3.4 "Memory protection", 3.4.1 "RDP (Read Out
//! Protection)", Figure 6, Table 19, Table 21):
//!
//! - The RDP option byte selects the level: 0xAA is level 0, 0xCC is level 2, any other value
//!   (or a byte whose complement does not match) is level 1. A new level takes effect when the
//!   option bytes are reloaded: a system reset (OBL_LAUNCH), or a power-on reset when a
//!   debugger has been active since the last one ("the option bytes loading is not
//!   sufficient").
//! - Level 1: while a debugger, a boot from RAM or the system memory (the ROM bootloader) is
//!   connected, the flash and the data EEPROM can be neither read (data or fetch) nor written.
//!   Booted from flash, the application runs normally. When level 1 is set through the
//!   bootloader (or a RAM boot or a debugger), user code runs only after a power-down or a
//!   wake-up from Standby.
//! - Level 1 to level 0 mass-erases the flash program memory AND the data EEPROM (and the
//!   backup registers), then rewrites the RDP byte to level 0. There is no other way down.
//! - Level 2 is permanent: debug disabled, the option bytes locked, and the MCU no longer boots
//!   the system memory (AN2606 Rev 70, 4.1), so the bootloader is gone for good.
//!
//! ROM bootloader, USART protocol (ST AN3155, command set and the Readout Protect, Readout
//! Unprotect, Get Version and Read Memory commands; the STM32L07x/L08x bootloader speaks USART
//! protocol V3.1 per AN2606 Rev 70, and reports 0x31):
//!
//! - With level 1 active the bootloader serves only Get (0x00), Get Version (0x01), Get ID
//!   (0x02) and Readout Unprotect (0x92). Every other command, Readout Protect included, is
//!   NACKed on its command byte and has no effect: Read Memory, Write Memory, Go and both Erase
//!   commands are refused. AN2606 confirms the command-stage NACK indirectly: a known
//!   limitation of an STM32L1 bootloader is that it sends two NACKs instead of one "when a Read
//!   Memory or Write Memory command is sent and the RDP level is active" (Table 157).
//! - Readout Protect: the host sends `82 7D`; the bootloader ACKs, enables the read protection
//!   in the option bytes, ACKs again, then generates a system reset to load them.
//! - Readout Unprotect: the host sends `92 6D`; the bootloader ACKs, disables the read
//!   protection, which mass-erases the flash (on the L0 also the data EEPROM, see above), ACKs
//!   again (a NACK instead means the protection stays active), then generates a system reset.
//!   AN2606 4.4 describes exactly this pair as the bootloader's way to mass-erase a part
//!   without a mass-erase command.
//! - Get Version replies with the version and two "option bytes" fixed at 0x00 for
//!   compatibility with the generic protocol: they do not carry the RDP state.
//!
//! # The probe
//!
//! The protection state is read by a refused-or-served command, not by Get Version (whose
//! option bytes say nothing, above): a Read Memory of 4 bytes at the flash base
//! (0x0800_0000, mapped on every STM32L0). ACK and data means level 0. A NACK on the command
//! byte means level 1, but a command byte corrupted on the line gets the same NACK, so a NACK
//! counts only when `Get ID` (served at every level) answers right after it, proving the link
//! and the bootloader's command parser are fine, and a second Read Memory is NACKed too. Before
//! that `Get ID` the input is drained for 20 ms, which absorbs the second NACK some
//! bootloaders send (above) even through the 16 ms latency timer of the FT231X bridge. A NACK
//! at the address or length stage is not protection (level 1 refuses at the command byte) and
//! propagates as an error. On a level-0 chip the probe is one Read Memory, a few milliseconds.
//!
//! [`chip::info`](crate::chip::info) reports the probe in
//! [`ChipInfo::readout_protected`](crate::chip::ChipInfo::readout_protected), and
//! [`flash::flash`] / [`flash::erase`] run it right after `Get ID`, so a protected board fails
//! with [`Error::ReadoutProtected`] before anything is sent that the bootloader would refuse.

use std::time::Duration;

use crate::bootloader::{Bootloader, Transport};
use crate::error::{Error, NackStage, Result};
use crate::flash::{self, Progress};
use crate::port::Port;
use crate::target;

/// Bound on the second ACK of Readout Protect / Unprotect. The option-byte write takes
/// milliseconds and the mass erase of the L0's flash + data EEPROM well under a second; the
/// bound is the one the page erase uses, generous on purpose because giving up early would
/// leave the outcome unknown.
const RDP_TIMEOUT: Duration = Duration::from_secs(30);

/// Pause after the second ACK before the next bootloader session: the device resets itself to
/// load the new option bytes, and the NRST pulse of the next session should not land in the
/// middle of that reload.
const RESET_SETTLE: Duration = Duration::from_millis(100);

/// Read Memory probes per decision: a protected chip must refuse every one of them, each
/// refusal followed by a healthy `Get ID`.
const PROBE_ATTEMPTS: u32 = 2;

/// Bytes read by the probe (one word at the flash base).
const PROBE_LEN: usize = 4;

/// How long to let unread bytes arrive before dropping them: after a refused Read Memory,
/// before `Get ID` (see the module docs: a possible second NACK), and after a failed `Get` in
/// [`chip::info`](crate::chip::info) (the rest of its reply). Covers the FT231X's 16 ms latency
/// timer, which can hold received bytes back that long.
pub(crate) const DRAIN: Duration = Duration::from_millis(20);

/// Whether readout protection (RDP level 1) is active, by the Read Memory probe described in
/// the module docs. Expects a live bootloader session (after `init`); sends only Read Memory
/// and `Get ID`, so it modifies nothing. `Ok(true)` = protected, `Ok(false)` = open; an error
/// means the link did not give a clean answer (never guessed as either state).
pub(crate) fn probe<T: Transport>(bl: &mut Bootloader<T>) -> Result<bool> {
    for _ in 0..PROBE_ATTEMPTS {
        match bl.read_block(target::FLASH_BASE, PROBE_LEN) {
            Ok(_) => return Ok(false),
            Err(Error::Nack {
                stage: NackStage::Command,
                ..
            }) => {
                // Refused, or the command byte was corrupted. Drain a possible second NACK,
                // then prove the bootloader is alive and in sync: Get ID is served at every
                // RDP level, so if it fails the NACK was the link, not the protection.
                bl.discard_input(DRAIN)?;
                bl.get_id()?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// The outcome of [`protect`] / [`unprotect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    /// STM32 product id from `Get ID` (e.g. `0x447` for the STM32L083CZ).
    pub id: u16,
    /// Readout protection found by the probe before the command.
    pub was_protected: bool,
    /// Readout protection read back afterwards by the probe in a new bootloader session (or,
    /// when nothing had to change, the state found before).
    pub readout_protected: bool,
}

impl Change {
    /// Whether the protection level changed. `false` means the chip already was in the
    /// requested state and no readout command was sent (so [`unprotect`] erased nothing).
    pub fn changed(&self) -> bool {
        self.was_protected != self.readout_protected
    }
}

/// Enable readout protection, RDP level 1 (never level 2): the opt-in provisioning step for a
/// production unit, after it has been flashed, verified and paired.
///
/// Sequence: enter the bootloader ([`flash::connect`]), `Get ID`, probe. Already protected:
/// nothing is sent (the bootloader would NACK Readout Protect anyway) and the result has
/// `changed() == false`. Otherwise Readout Protect (`82 7D`, ACK, [`Progress::ReadoutChanging`],
/// ACK once the option bytes are written); the device then resets itself, so the bootloader
/// is re-entered exactly like the first time and the probe reads the new state back.
/// [`Error::ReadoutUnchanged`] if it still reads unprotected. Every path ends with a reset into
/// the application (best effort on errors).
///
/// Undoing this costs everything stored on the chip: [`unprotect`] mass-erases the flash AND
/// the data EEPROM. The application keeps running from flash at level 1, but RM0367 3.4.1
/// says that after level 1 is set through the bootloader, user code runs only after a
/// power-down: the board has to be power-cycled once.
///
/// Emits [`Progress::Connecting`] / [`Progress::ConnectError`] (both sessions),
/// [`Progress::ChipIdentified`], [`Progress::ReadoutProbed`] (before and after) and
/// [`Progress::ReadoutChanging`].
pub fn protect(port: &mut Port, progress: &mut dyn FnMut(Progress)) -> Result<Change> {
    set_level(port, true, progress)
}

/// Remove readout protection: RDP level 1 back to level 0. **Destructive**: the regression
/// mass-erases the whole flash (the firmware) AND the data EEPROM (radio keys, pairing,
/// settings, TX counters; RM0367 3.4.1), so the node must be re-flashed and re-paired.
///
/// Same sequence as [`protect`] with Readout Unprotect (`92 6D`; the second ACK comes after
/// the mass erase). A chip that is not protected is left alone: nothing is sent, nothing is
/// erased, and the result has `changed() == false`. [`Error::ReadoutUnchanged`] if the
/// read-back still finds it protected. The device is reset into the application at the end;
/// with the flash erased there is nothing to run until it is re-flashed.
pub fn unprotect(port: &mut Port, progress: &mut dyn FnMut(Progress)) -> Result<Change> {
    set_level(port, false, progress)
}

/// Shared body of [`protect`] (`protect == true`) and [`unprotect`].
fn set_level(port: &mut Port, protect: bool, progress: &mut dyn FnMut(Progress)) -> Result<Change> {
    flash::connect(port, progress)?;
    // Up to the readout command nothing has changed: on any error, put the intact app back.
    let (id, was_protected) = match identify(&mut Bootloader::new(port), progress, true) {
        Ok(found) => found,
        Err(e) => {
            let _ = port.reset_into_app();
            return Err(e);
        }
    };
    if was_protected == protect {
        port.reset_into_app()?;
        return Ok(Change {
            id,
            was_protected,
            readout_protected: was_protected,
        });
    }

    let sent = {
        let mut bl = Bootloader::new(port);
        let mut accepted = || progress(Progress::ReadoutChanging { protect });
        if protect {
            bl.readout_protect(RDP_TIMEOUT, &mut accepted)
        } else {
            bl.readout_unprotect(RDP_TIMEOUT, &mut accepted)
        }
    };
    if let Err(e) = sent {
        // A command-stage NACK changed nothing; a failure after the first ACK leaves the level
        // unknown (the caller reads it with `chip::info`). Either way this session is over.
        let _ = port.reset_into_app();
        return Err(e);
    }

    // The device resets itself to load the new option bytes. Re-enter the bootloader exactly
    // like the first time (the reset pulse with BOOT0 high, 0x7F, retries) and read back.
    std::thread::sleep(RESET_SETTLE);
    flash::connect(port, progress)?;
    let readout_protected = match identify(&mut Bootloader::new(port), progress, false) {
        Ok((_, protected)) => protected,
        Err(e) => {
            let _ = port.reset_into_app();
            return Err(e);
        }
    };
    port.reset_into_app()?;
    if readout_protected != protect {
        return Err(Error::ReadoutUnchanged { protect });
    }
    Ok(Change {
        id,
        was_protected,
        readout_protected,
    })
}

/// `Get ID` then [`probe`], reporting [`Progress::ReadoutProbed`] (and
/// [`Progress::ChipIdentified`] when `announce`, i.e. not for a read-back).
fn identify<T: Transport>(
    bl: &mut Bootloader<T>,
    progress: &mut dyn FnMut(Progress),
    announce: bool,
) -> Result<(u16, bool)> {
    let id = bl.get_id()?;
    if announce {
        progress(Progress::ChipIdentified { id });
    }
    let readout_protected = probe(bl)?;
    progress(Progress::ReadoutProbed { readout_protected });
    Ok((id, readout_protected))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bootloader::tests::ScriptedTransport;
    use crate::flash::{FlashOptions, no_progress};
    use crate::port::tests::fake_port;

    const ACK: u8 = 0x79;
    const NACK: u8 = 0x1F;

    /// The modem-line tail of `Port::reset_into_app`: RESET asserted with BOOT0 low, then
    /// released.
    const APP_RESET_TAIL: [(char, bool); 3] = [('R', true), ('D', false), ('R', false)];

    /// Device side of a bootloader entry: the ACK to 0x7F.
    pub(crate) const INIT: [u8; 1] = [ACK];
    /// Device side of `Get ID` on an STM32L083CZ: ACK, N=1, 0x04 0x47, ACK.
    pub(crate) const GET_ID_447: [u8; 5] = [ACK, 0x01, 0x04, 0x47, ACK];
    /// Device side of the probe on an open chip: Read Memory command, address and length
    /// ACKed, then the 4 bytes at the flash base (an initial SP, 0x2000_5000).
    pub(crate) const PROBE_OPEN: [u8; 7] = [ACK, ACK, ACK, 0x00, 0x50, 0x00, 0x20];
    /// Device side of the probe on a protected chip: Read Memory NACKed, `Get ID` served,
    /// twice.
    pub(crate) const PROBE_PROTECTED: [u8; 12] = [
        NACK, ACK, 0x01, 0x04, 0x47, ACK, // first refusal, healthy Get ID
        NACK, ACK, 0x01, 0x04, 0x47, ACK, // second refusal, healthy Get ID
    ];

    /// Host side of the probe on an open chip: `11 EE`, the flash-base address frame, N=3.
    const PROBE_OPEN_WIRE: [u8; 9] = [0x11, 0xEE, 0x08, 0x00, 0x00, 0x00, 0x08, 0x03, 0xFC];
    /// Host side of the probe on a protected chip: Read Memory, Get ID, twice.
    const PROBE_PROTECTED_WIRE: [u8; 8] = [0x11, 0xEE, 0x02, 0xFD, 0x11, 0xEE, 0x02, 0xFD];

    const PROTECT_FRAME: [u8; 2] = [0x82, 0x7D];
    const UNPROTECT_FRAME: [u8; 2] = [0x92, 0x6D];

    pub(crate) fn script(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// How many times the command frame `frame` occurs in `wire`.
    pub(crate) fn frames(wire: &[u8], frame: [u8; 2]) -> usize {
        wire.windows(2).filter(|w| *w == frame).count()
    }

    // ---- the probe -------------------------------------------------------------------

    #[test]
    fn probe_reads_an_open_chip() {
        let mut t = ScriptedTransport::new();
        t.push_bytes(&PROBE_OPEN);
        assert!(!probe(&mut Bootloader::new(&mut t)).unwrap());
        assert_eq!(t.written(), &PROBE_OPEN_WIRE);
    }

    #[test]
    fn probe_reads_a_protected_chip() {
        let mut t = ScriptedTransport::new();
        t.push_bytes(&PROBE_PROTECTED);
        assert!(probe(&mut Bootloader::new(&mut t)).unwrap());
        assert_eq!(t.written(), &PROBE_PROTECTED_WIRE);
    }

    /// The bootloader variant that sends two NACKs for a refused Read Memory (AN2606 Table
    /// 157): the stray second NACK is drained before `Get ID`, so it is not read as Get ID's
    /// ACK stage.
    #[test]
    fn probe_tolerates_a_double_nack() {
        let mut t = ScriptedTransport::new();
        for _ in 0..PROBE_ATTEMPTS {
            t.push_nack();
            t.push_stale(NACK);
            t.push_bytes(&GET_ID_447);
        }
        assert!(probe(&mut Bootloader::new(&mut t)).unwrap());
    }

    /// One corrupted command byte is not protection: the retry is served.
    #[test]
    fn probe_one_refusal_then_data_is_open() {
        let mut t = ScriptedTransport::new();
        t.push_nack();
        t.push_bytes(&GET_ID_447);
        t.push_bytes(&PROBE_OPEN);
        assert!(!probe(&mut Bootloader::new(&mut t)).unwrap());
    }

    /// A refusal followed by an unhealthy `Get ID` is a link problem, reported as an error
    /// rather than guessed as "protected".
    #[test]
    fn probe_refusal_without_a_healthy_get_id_is_an_error() {
        let mut t = ScriptedTransport::new();
        t.push_nack(); // Read Memory refused
        t.push_nack(); // ... and Get ID too: not the protection
        let err = probe(&mut Bootloader::new(&mut t)).unwrap_err();
        assert!(matches!(err, Error::Nack { .. }), "got {err:?}");
    }

    /// Level 1 refuses at the command byte; a NACK to the address is something else and
    /// propagates.
    #[test]
    fn probe_address_nack_is_an_error() {
        let mut t = ScriptedTransport::new();
        t.push_ack();
        t.push_nack();
        let err = probe(&mut Bootloader::new(&mut t)).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Nack {
                    stage: NackStage::Payload,
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    // ---- protect -----------------------------------------------------------------------

    /// Both ACKs, then the device resets itself; a new session reads the chip back as
    /// protected. The exact bytes on the wire: two bootloader sessions, one Readout Protect.
    #[test]
    fn protect_happy_path() {
        let (mut port, state) = fake_port(&script(&[
            &INIT,
            &GET_ID_447,
            &PROBE_OPEN,
            &[ACK, ACK], // Readout Protect: accepted, option bytes written
            &INIT,
            &GET_ID_447,
            &PROBE_PROTECTED,
        ]));
        let mut seen = Vec::new();
        let change = protect(&mut port, &mut |p| seen.push(p)).unwrap();
        assert_eq!(
            change,
            Change {
                id: 0x447,
                was_protected: false,
                readout_protected: true
            }
        );
        assert!(change.changed());

        let st = state.lock().unwrap();
        let wire = script(&[
            &[0x7F, 0x02, 0xFD],
            &PROBE_OPEN_WIRE,
            &PROTECT_FRAME,
            &[0x7F, 0x02, 0xFD],
            &PROBE_PROTECTED_WIRE,
        ]);
        assert_eq!(st.written, wire);
        assert!(st.lines.ends_with(&APP_RESET_TAIL), "reset into app");
        assert_eq!(
            st.timeout,
            crate::port::DEFAULT_TIMEOUT,
            "long timeout rolled back"
        );
        let probes: Vec<bool> = seen
            .iter()
            .filter_map(|p| match p {
                Progress::ReadoutProbed { readout_protected } => Some(*readout_protected),
                _ => None,
            })
            .collect();
        assert_eq!(probes, [false, true], "probed before and after");
        assert!(
            seen.iter()
                .any(|p| matches!(p, Progress::ReadoutChanging { protect: true }))
        );
    }

    /// Readout Protect NACKed on its command byte: nothing changed, no second session, the
    /// app is put back.
    #[test]
    fn protect_command_nack_is_an_error_and_restores_the_app() {
        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447, &PROBE_OPEN, &[NACK]]));
        let err = protect(&mut port, &mut no_progress).unwrap_err();
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
        let st = state.lock().unwrap();
        assert_eq!(frames(&st.written, PROTECT_FRAME), 1);
        assert_eq!(
            st.written.iter().filter(|&&b| b == 0x7F).count(),
            1,
            "one session"
        );
        assert!(st.lines.ends_with(&APP_RESET_TAIL));
    }

    /// The second ACK is a NACK: the option-byte update failed (Operation stage).
    #[test]
    fn protect_operation_nack_is_an_error() {
        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447, &PROBE_OPEN, &[ACK, NACK]]));
        let err = protect(&mut port, &mut no_progress).unwrap_err();
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
        assert!(state.lock().unwrap().lines.ends_with(&APP_RESET_TAIL));
    }

    /// An already protected chip: Readout Protect is not sent (it would be NACKed), the
    /// result says nothing changed.
    #[test]
    fn protect_an_already_protected_chip_sends_nothing() {
        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447, &PROBE_PROTECTED]));
        let change = protect(&mut port, &mut no_progress).unwrap();
        assert!(change.was_protected && change.readout_protected && !change.changed());
        let st = state.lock().unwrap();
        assert_eq!(frames(&st.written, PROTECT_FRAME), 0);
        assert!(st.lines.ends_with(&APP_RESET_TAIL));
    }

    /// Two ACKs but the read-back still finds the chip open (e.g. a debugger attached since
    /// power-on, so the option bytes did not load): an error, not a silent success.
    #[test]
    fn protect_that_does_not_read_back_is_an_error() {
        let (mut port, state) = fake_port(&script(&[
            &INIT,
            &GET_ID_447,
            &PROBE_OPEN,
            &[ACK, ACK],
            &INIT,
            &GET_ID_447,
            &PROBE_OPEN,
        ]));
        let err = protect(&mut port, &mut no_progress).unwrap_err();
        assert!(
            matches!(err, Error::ReadoutUnchanged { protect: true }),
            "got {err:?}"
        );
        assert!(err.to_string().contains("power-cycle"), "{err}");
        assert!(state.lock().unwrap().lines.ends_with(&APP_RESET_TAIL));
    }

    // ---- unprotect ---------------------------------------------------------------------

    /// Both ACKs (the second after the mass erase), the device resets, a new session reads
    /// the chip back as open.
    #[test]
    fn unprotect_happy_path() {
        let (mut port, state) = fake_port(&script(&[
            &INIT,
            &GET_ID_447,
            &PROBE_PROTECTED,
            &[ACK, ACK], // Readout Unprotect: accepted, mass erase + level 0 done
            &INIT,
            &GET_ID_447,
            &PROBE_OPEN,
        ]));
        let mut seen = Vec::new();
        let change = unprotect(&mut port, &mut |p| seen.push(p)).unwrap();
        assert_eq!(
            change,
            Change {
                id: 0x447,
                was_protected: true,
                readout_protected: false
            }
        );
        let st = state.lock().unwrap();
        let wire = script(&[
            &[0x7F, 0x02, 0xFD],
            &PROBE_PROTECTED_WIRE,
            &UNPROTECT_FRAME,
            &[0x7F, 0x02, 0xFD],
            &PROBE_OPEN_WIRE,
        ]);
        assert_eq!(st.written, wire);
        assert!(st.lines.ends_with(&APP_RESET_TAIL));
        assert!(
            seen.iter()
                .any(|p| matches!(p, Progress::ReadoutChanging { protect: false }))
        );
    }

    /// `unprotect` on an open chip erases nothing: no Readout Unprotect, no erase.
    #[test]
    fn unprotect_an_open_chip_erases_nothing() {
        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447, &PROBE_OPEN]));
        let change = unprotect(&mut port, &mut no_progress).unwrap();
        assert!(!change.was_protected && !change.changed());
        let st = state.lock().unwrap();
        assert_eq!(frames(&st.written, UNPROTECT_FRAME), 0);
        assert_eq!(frames(&st.written, [0x44, 0xBB]), 0);
    }

    /// A NACK in place of the second ACK: per AN3155 the protection stays active.
    #[test]
    fn unprotect_operation_nack_is_an_error() {
        let (mut port, _state) = fake_port(&script(&[&INIT, &GET_ID_447, &PROBE_PROTECTED, &[ACK, NACK]]));
        let err = unprotect(&mut port, &mut no_progress).unwrap_err();
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
    }

    // ---- nothing else sends 0x82 / 0x92 ------------------------------------------------

    /// Dynamic half: every other entry point, run to completion on an open chip (and to its
    /// readout-protected refusal on a protected one), never puts a Readout Protect or Readout
    /// Unprotect command frame on the wire.
    #[test]
    fn no_other_entry_point_sends_the_readout_commands() {
        let fw = [0xAAu8; 8];
        let write_and_verify: &[u8] = &[
            ACK, ACK, ACK, ACK, ACK, ACK, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        // Extended Erase of the 192 KiB part: 1536 pages in 80-page chunks, two ACKs each.
        let full_erase =
            vec![ACK; 2 * target::pages_in(target::MAX_FLASH_SIZE).div_ceil(target::ERASE_CHUNK)];
        let get = [ACK, 0x02, 0x31, 0x00, 0x02, ACK];

        let mut wires = Vec::new();
        let (mut port, state) = fake_port(&script(&[
            &INIT,
            &GET_ID_447,
            &PROBE_OPEN,
            &[ACK, ACK], // footprint erase, one chunk
            write_and_verify,
        ]));
        flash::flash(&mut port, &fw, &FlashOptions::default(), &mut no_progress).unwrap();
        wires.push(("flash", state.lock().unwrap().written.clone()));

        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447, &PROBE_OPEN, &full_erase]));
        flash::erase(&mut port, &mut no_progress).unwrap();
        wires.push(("erase", state.lock().unwrap().written.clone()));

        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447, &get, &PROBE_OPEN]));
        crate::chip::info(&mut port, &mut no_progress).unwrap();
        wires.push(("info", state.lock().unwrap().written.clone()));

        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447, &get, &PROBE_PROTECTED]));
        crate::chip::info(&mut port, &mut no_progress).unwrap();
        wires.push(("info, protected", state.lock().unwrap().written.clone()));

        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447, &PROBE_PROTECTED]));
        flash::flash(&mut port, &fw, &FlashOptions::default(), &mut no_progress).unwrap_err();
        wires.push(("flash, protected", state.lock().unwrap().written.clone()));

        let (mut port, state) = fake_port(&script(&[&INIT, &GET_ID_447, &PROBE_PROTECTED]));
        flash::erase(&mut port, &mut no_progress).unwrap_err();
        wires.push(("erase, protected", state.lock().unwrap().written.clone()));

        for (what, wire) in &wires {
            assert!(wire.len() > 3, "{what}: the session ran");
            assert_eq!(frames(wire, PROTECT_FRAME), 0, "{what}: Readout Protect sent");
            assert_eq!(frames(wire, UNPROTECT_FRAME), 0, "{what}: Readout Unprotect sent");
        }
    }

    /// Static half: in the library code (every `src/*.rs`, each up to its test module), the
    /// two opcodes are named only where `bootloader.rs` defines and sends them, and the two
    /// crate-private methods that send them are called only from this module, once each. A
    /// new caller, in any file present or future, fails this test and has to be justified here.
    #[test]
    fn readout_commands_are_sent_only_by_protect_and_unprotect() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut seen = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let src = std::fs::read_to_string(&path).unwrap();
            let lib = src.split("#[cfg(test)]").next().unwrap();
            let count = |needle: &str| lib.matches(needle).count();
            let (opcodes, calls) = match name.as_str() {
                "bootloader.rs" => (2, 0), // each constant: its definition and its one send
                "rdp.rs" => (0, 1),        // `set_level` calls each method once
                _ => (0, 0),
            };
            seen.push(name.clone());
            assert_eq!(count("CMD_READOUT_PROTECT"), opcodes, "{name}");
            assert_eq!(count("CMD_READOUT_UNPROTECT"), opcodes, "{name}");
            assert_eq!(count(".readout_protect("), calls, "{name}");
            assert_eq!(count(".readout_unprotect("), calls, "{name}");
        }
        for name in ["bootloader.rs", "rdp.rs", "flash.rs", "chip.rs"] {
            assert!(seen.iter().any(|s| s == name), "{name} scanned");
        }
    }
}
