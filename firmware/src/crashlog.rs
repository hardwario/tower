//! Crash breadcrumb: a fault record that survives the reset.
//!
//! The panic / HardFault handlers can emit one framed error over the console, but only when
//! USB is attached. A battery node that faults in the field used to halt forever with no
//! record. Now the handlers write the crash text into a **reset-surviving `.uninit` RAM
//! record** (the [`bootguard`](crate::bootguard) pattern: retained across a warm reset, never
//! persisted, so zero EEPROM wear) and reset. The next boot picks the record up
//! ([`take`], called via `console::emit_crash_report` from the [`app!`](crate::app) macro),
//! logs it as an error frame, and caches it for `/system/crash print`, so the crash is
//! visible on the wire after the unit has already recovered, and on demand for the rest of
//! the power-on.
//!
//! A cold boot (random RAM) is told from a real record by a magic marker, exactly like the
//! boot-loop guard; a brown-out that loses RAM retention reads as "no crash", the safe default.
//!
//! Two failure modes never reach a handler, so other code writes their record instead:
//! * **stack overflow**: with flip-link an overflow pushes SP below the RAM base; the HardFault
//!   entry's own exception-frame stacking faults again and the ARMv6-M core **locks up** (no
//!   handler runs; only an armed IWDG recovers it). The next boot's
//!   [`stack::paint`](crate::stack::paint) inspects the previous boot's paint and, on an IWDG reset
//!   with the paint consumed to within [`OVERFLOW_MARGIN`](crate::stack::OVERFLOW_MARGIN) of the
//!   bottom, records a `stack-overflow` crash;
//! * **wedge**: the [`watchdog`](crate::watchdog) feeder records a `watchdog` crash when its
//!   liveness gate withholds the pet. Reported only if the reset really was the IWDG (a loop that
//!   recovered before it fired retracts the record).

use core::cell::RefCell;
use core::ptr::addr_of_mut;

use critical_section::Mutex;

/// Capacity of the stored crash text; it matches the console's `MAX_MSG` so the breadcrumb can
/// hold exactly what the panic handler would have emitted over the wire.
pub const MSG_CAP: usize = 192;

/// Marks [`RECORD`] as a live crash record (vs cold-boot RAM garbage).
const MAGIC: u32 = 0xC7A5_FEED;

/// The faulting path that wrote the record.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The Rust panic handler (`panic!`, slice OOB, arithmetic checks, …).
    Panic,
    /// The Cortex-M HardFault exception (bad memory access, invalid instruction, …).
    HardFault,
    /// A probable stack overflow: inferred at boot from the previous boot's stack paint + an
    /// IWDG reset (the overflow itself locks the core up before any handler can run).
    StackOverflow,
    /// The watchdog feeder withheld the pet because the app's liveness check-ins went stale.
    Watchdog,
}

impl Kind {
    /// Short human-readable tag (used in the boot report and the shell command).
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Panic => "panic",
            Kind::HardFault => "hardfault",
            Kind::StackOverflow => "stack-overflow",
            Kind::Watchdog => "watchdog",
        }
    }

    fn code(self) -> u32 {
        match self {
            Kind::Panic => 1,
            Kind::HardFault => 2,
            Kind::StackOverflow => 3,
            Kind::Watchdog => 4,
        }
    }

    fn from_code(c: u32) -> Self {
        match c {
            2 => Kind::HardFault,
            3 => Kind::StackOverflow,
            4 => Kind::Watchdog,
            _ => Kind::Panic,
        }
    }
}

/// The raw `.uninit` record. Plain `u32`s + a byte array: no invalid bit patterns, so a
/// volatile read of pre-init RAM is well-defined (see `bootguard::STATE` for the argument).
#[repr(C)]
#[derive(Clone, Copy)]
struct Record {
    magic: u32,
    /// 1 = panic, 2 = HardFault, 3 = stack overflow, 4 = watchdog (kept as a raw int; an enum
    /// would have invalid patterns).
    kind: u32,
    len: u32,
    msg: [u8; MSG_CAP],
}

#[unsafe(link_section = ".uninit.TOWER_CRASHLOG")]
static mut RECORD: Record = Record {
    magic: 0,
    kind: 0,
    len: 0,
    msg: [0; MSG_CAP],
};

/// The record [`take`] recovered on this boot, kept for `/system/crash print`. Written once
/// at start-up (before the shell serves), read-only afterwards.
static LAST: Mutex<RefCell<Option<Crash>>> = Mutex::new(RefCell::new(None));

/// A recovered crash record, copied out of the `.uninit` RAM.
#[derive(Clone)]
pub struct Crash {
    pub kind: Kind,
    len: usize,
    msg: [u8; MSG_CAP],
}

impl Crash {
    /// The crash text (the panic message / fault PC+LR line). Always valid UTF-8 in practice
    /// (the handlers wrote it from a `str`); garbage that survived the magic check by
    /// coincidence degrades to a placeholder rather than a panic in the reporter.
    pub fn message(&self) -> &str {
        core::str::from_utf8(&self.msg[..self.len]).unwrap_or("<corrupt crash record>")
    }
}

/// Store a crash record (clipped to [`MSG_CAP`] at a char boundary). Called from the panic /
/// HardFault handlers (single-threaded, executor stopped), and, via [`record_unless_fault`],
/// from boot code and the watchdog feeder, whose only possible race is a panic/fault preempting
/// the write, which then overwrites it with its own (more specific) record.
pub(crate) fn record(kind: Kind, msg: &str) {
    let bytes = msg.as_bytes();
    let mut len = bytes.len().min(MSG_CAP);
    while len > 0 && !msg.is_char_boundary(len) {
        len -= 1;
    }
    let mut rec = Record {
        magic: MAGIC,
        kind: kind.code(),
        len: len as u32,
        msg: [0; MSG_CAP],
    };
    rec.msg[..len].copy_from_slice(&bytes[..len]);
    // SAFETY: volatile write to the retained static; see the race note above.
    unsafe { addr_of_mut!(RECORD).write_volatile(rec) };
}

/// The kind of the record currently held (not consumed), if any.
fn pending_kind() -> Option<Kind> {
    // SAFETY: plain-integer fields of the retained static, volatile reads (see `take`).
    let (magic, kind) = unsafe {
        (
            addr_of_mut!(RECORD.magic).read_volatile(),
            addr_of_mut!(RECORD.kind).read_volatile(),
        )
    };
    (magic == MAGIC).then(|| Kind::from_code(kind))
}

/// [`record`], but never overwriting a pending `panic`/`hardfault` record; those are direct
/// evidence from a handler, while `stack-overflow` / `watchdog` are inferences.
pub(crate) fn record_unless_fault(kind: Kind, msg: &str) {
    if !matches!(pending_kind(), Some(Kind::Panic | Kind::HardFault)) {
        record(kind, msg);
    }
}

/// Retract a pending record of `kind` (the watchdog feeder, once the loop recovered).
pub(crate) fn clear_kind(kind: Kind) {
    if pending_kind() == Some(kind) {
        // SAFETY: as in `record`.
        unsafe { addr_of_mut!(RECORD.magic).write_volatile(0) };
    }
}

/// Recover the previous boot's crash record, if any, and clear it, so it is reported once.
/// Call once, early in start-up (the `app!` macro does, via `console::emit_crash_report`);
/// the record is then cached for [`last`].
pub fn take() -> Option<Crash> {
    let p = addr_of_mut!(RECORD);
    // SAFETY: single-threaded start-up; volatile access to RAM retained across a warm reset
    // (or arbitrary-but-valid bytes on a cold boot, which the magic rejects).
    let rec = unsafe { p.read_volatile() };
    if rec.magic != MAGIC {
        return None;
    }
    unsafe {
        p.write_volatile(Record {
            magic: 0,
            kind: 0,
            len: 0,
            msg: [0; MSG_CAP],
        })
    };
    let kind = Kind::from_code(rec.kind);
    // A `watchdog` record is only an *intent* ("pet withheld"); trust it only if the reset that
    // ended that boot really was the IWDG (the RCC_CSR snapshot `board::init` took this boot).
    if kind == Kind::Watchdog && !embassy_stm32::pac::rcc::regs::Csr(crate::board::preinit_csr()).iwdgrstf() {
        return None;
    }
    let crash = Crash {
        kind,
        len: (rec.len as usize).min(MSG_CAP),
        msg: rec.msg,
    };
    critical_section::with(|cs| {
        *LAST.borrow(cs).borrow_mut() = Some(crash.clone());
    });
    Some(crash)
}

/// The crash recovered at this boot, if any: what `/system/crash print` reports. `None` after
/// a clean boot (or a brown-out that lost RAM retention).
pub fn last() -> Option<Crash> {
    critical_section::with(|cs| LAST.borrow(cs).borrow().clone())
}
