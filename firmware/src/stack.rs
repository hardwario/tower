//! Boot-time stack painting + a runtime high-water read: a *measured* answer to "how deep does
//! the stack actually go?" on this 20 KB part, replacing the ~7.5 KB estimate the gateway budget
//! carries (docs/gateway.md) with a bench number.
//!
//! The stack region is `[_stack_end, _stack_start)`, the linker symbols cortex-m-rt defines for
//! exactly this (its own `paint-stack` feature paints the same span), so the bounds are right in
//! **both** layouts:
//!
//! * **flip-link** (this repo's `.cargo/config.toml`): the stack sits at the **bottom** of RAM,
//!   `[RAM origin, start of statics)`, and grows down toward the RAM origin, so an overflow faults
//!   on the underflow instead of silently corrupting `.bss`;
//! * the **default cortex-m-rt** layout (a downstream crate linking without flip-link): statics
//!   (`.data`/`.bss`/`.uninit`, incl. the crash breadcrumb) sit at the RAM origin and the stack is
//!   `[end of statics, RAM end)`. Painting from the RAM origin there (as this module once did)
//!   would overwrite every static at boot → a HardFault boot loop. (If such a crate also adds a
//!   heap allocator at `__sheap` = `_stack_end`, the paint is harmless, since it runs before any
//!   allocation, but the high-water read would count heap blocks as stack.)
//!
//! [`paint`] fills the free stem of that region with a sentinel word once, from the shallow boot
//! frame; as execution deepens (including ISR frames, since every task polls on the one executor
//! stack), the sentinels get overwritten. [`used`] then scans from the bottom for the first
//! surviving sentinel: the lowest address the stack ever reached. Because `used = _stack_start −
//! low_water`, the never-painted top region (live at paint time) is still counted, so there is no
//! systematic undercount. Read it over the console with `system stack print` after driving the
//! deepest paths (the HIL gateway stack test does exactly that).
//!
//! **Overflow forensics.** A flip-link overflow never reaches a handler: SP is below the RAM
//! origin, the HardFault entry's exception-frame stacking faults again, and the ARMv6-M core
//! **locks up** (only an armed IWDG resets it). The paint is the evidence that survives: RAM is
//! retained across that reset, so before re-painting, [`paint`] looks at the *previous* boot's
//! paint: if the deepest reach got within [`OVERFLOW_MARGIN`] of the bottom and the reset was the
//! IWDG, it files a `stack-overflow` [`crashlog`](crate::crashlog) record, which the boot reporter
//! then surfaces like any panic (`system crash print`).

use core::ptr::{self, addr_of_mut};
use core::sync::atomic::{AtomicU32, Ordering};

/// Sentinel word painted into the free stack. `0xAAAA_AAAA` (alternating bits) is vanishingly
/// unlikely as a genuine stack value, so a surviving one reliably marks never-touched space.
const SENTINEL: u32 = 0xAAAA_AAAA;

/// Bytes left unpainted just below the live frame at [`paint`] time, so the paint loop can never
/// clobber its own return address / locals.
const PAINT_GUARD: usize = 128;

/// How close to the bottom the previous boot's deepest reach must have come for an IWDG reset to
/// be blamed on a stack overflow. Not zero: an overflowing prologue moves SP by a whole frame
/// (`sub sp, #N`, up to ~0.8 KB for the KV flip's plan + record buffer) *before* its first store
/// faults below the RAM origin, so the words just above the bottom can keep their sentinel even
/// though the stack went past them. A boot that legitimately got this close was itself a
/// near-overflow (docs/gateway.md keeps ~1 KB of headroom over the measured peaks).
pub const OVERFLOW_MARGIN: usize = 1024;

/// Marks [`PAINTED`] as written by a previous boot's [`paint`] (vs cold-boot RAM garbage).
const PAINT_MAGIC: u32 = 0x5747_A1E7;

/// `[magic, bottom, top]` of the last [`paint`], in reset-surviving `.uninit` RAM (the
/// `bootguard`/`crashlog` pattern): lets the next boot tell "the previous boot painted this exact
/// region and RAM survived" from a cold boot or a different firmware layout.
#[unsafe(link_section = ".uninit.TOWER_STACKPAINT")]
static mut PAINTED: [u32; 3] = [0; 3];

/// The previous boot's peak stack use (bytes), measured from its paint before [`paint`] re-painted;
/// `u32::MAX` = unknown (cold boot, no previous paint, or a different layout).
static PREV_USED: AtomicU32 = AtomicU32::new(u32::MAX);

unsafe extern "C" {
    /// Linker symbol (cortex-m-rt; flip-link redefines it): its ADDRESS is the initial SP = the
    /// top of the stack region, which grows down from here.
    static _stack_start: u8;
    /// Linker symbol (cortex-m-rt; flip-link redefines it): its ADDRESS is the stack's low bound:
    /// the RAM origin under flip-link, the end of the statics under the default layout.
    static _stack_end: u8;
}

#[inline]
fn stack_top() -> usize {
    &raw const _stack_start as usize
}

#[inline]
fn stack_bottom() -> usize {
    // Rounded up to a word: the paint/scan loops step whole words.
    (&raw const _stack_end as usize + 3) & !3
}

/// Whether the stack sits **below** the statics (the flip-link layout), where an overflow faults
/// instead of trampling `.bss`, so the boot-time overflow forensics are meaningful.
fn below_statics() -> bool {
    stack_top() <= addr_of_mut!(PAINTED) as usize
}

/// Total stack region size in bytes (`_stack_start − _stack_end`).
#[must_use]
pub fn total() -> usize {
    stack_top() - stack_bottom()
}

/// Paint the free stack below the current frame with [`SENTINEL`]. Call **once**, as early as
/// possible from a shallow frame (the `app!` entry does, before `Board::take`), so the paint
/// covers everything the deeper call graph + ISR frames will later use.
///
/// First, if the previous boot painted the same region and RAM survived the reset, record how deep
/// that boot's stack went ([`previous_used`]) and, on an IWDG reset with the paint consumed to
/// within [`OVERFLOW_MARGIN`] of the bottom, file a `stack-overflow` crash record (see the module
/// docs). The reset cause is read straight from `RCC_CSR`: this runs before `board::init`
/// snapshots and clears it.
///
/// Safe in practice: at the boot entry the stack is nearly empty, so `[_stack_end, sp − guard)` is
/// all genuinely-free RAM below the live frame (task futures live in statics, not on this stack).
pub fn paint() {
    let (bottom, top) = (stack_bottom(), stack_top());
    let p = addr_of_mut!(PAINTED);
    // SAFETY: single-threaded start-up; volatile access to a plain-`u32` retained static (garbage
    // on a cold boot, rejected by the magic + bounds check).
    let prev = unsafe { p.read_volatile() };
    // Only a reset the firmware or the IWDG caused guarantees nothing else ran in between: a pin
    // reset may be a UART-bootloader session (`tower flash`), and the ROM bootloader works in the
    // low RAM (this very paint region), so its leftovers would read as a deep stack.
    let csr = embassy_stm32::pac::RCC.csr().read();
    let iwdg_reset = csr.iwdgrstf();
    if prev == [PAINT_MAGIC, bottom as u32, top as u32] && (iwdg_reset || csr.sftrstf()) {
        // This boot has only used the top of the stack so far, so the lowest non-sentinel word is
        // still the previous boot's deepest reach.
        let low = low_water();
        PREV_USED.store((top - low) as u32, Ordering::Relaxed);
        let free = low - bottom;
        if iwdg_reset && below_statics() && free < OVERFLOW_MARGIN {
            // ≤ ~100 B worst case, sized with room so no piece is rejected (a bounded `write!`
            // drops a whole piece that doesn't fit).
            let mut msg = heapless::String::<128>::new();
            let _ = core::fmt::Write::write_fmt(
                &mut msg,
                format_args!(
                    "probable stack overflow: IWDG reset, stack reached {free} B from the bottom \
                     ({} B used)",
                    top - low
                ),
            );
            crate::crashlog::record_unless_fault(crate::crashlog::Kind::StackOverflow, &msg);
        }
    }

    let sp = cortex_m::register::msp::read() as usize;
    let hi = sp.saturating_sub(PAINT_GUARD).min(top);
    let mut a = bottom;
    while a + 4 <= hi {
        // SAFETY: [_stack_end, hi) ⊂ the free stack region below the live frame at boot;
        // word-aligned; never a static (the stack region excludes them in either layout).
        unsafe { ptr::write_volatile(a as *mut u32, SENTINEL) };
        a += 4;
    }
    unsafe { p.write_volatile([PAINT_MAGIC, bottom as u32, top as u32]) };
}

/// Lowest address the stack ever reached (its deepest point): the first word, scanning UP from
/// the bottom, that is no longer the sentinel. Returns the bottom if the whole paint was consumed
/// (a near-overflow; flip-link would have faulted before a true overflow).
fn low_water() -> usize {
    let top = stack_top();
    let mut p = stack_bottom();
    while p + 4 <= top {
        // SAFETY: reading within the stack region; word-aligned and < top.
        if unsafe { ptr::read_volatile(p as *const u32) } != SENTINEL {
            break;
        }
        p += 4;
    }
    p
}

/// Peak stack **used** in bytes (`_stack_start − low_water`): the high-water mark to weigh against
/// the 8 KB budget floor. `0` if [`paint`] was never called (the region is unpainted, so the very
/// first word differs from the sentinel only by chance; treat a suspiciously-low read as "not
/// painted").
#[must_use]
pub fn used() -> usize {
    stack_top() - low_water()
}

/// Stack still **free** in bytes at the deepest point reached (`low_water − _stack_end`). Near
/// zero means the paint was almost fully consumed: a near-overflow warning.
#[must_use]
pub fn free() -> usize {
    low_water() - stack_bottom()
}

/// The **previous** boot's peak stack use in bytes, read from its paint at this boot's [`paint`]
/// (RAM survives a warm reset). Known only after a software (`system reboot`, panic, HardFault)
/// or IWDG reset; `None` after a cold boot / brown-out / pin reset (possibly a bootloader
/// session), or when the previous image had a different stack layout. Lets a field unit report
/// the high-water of the run that just ended (e.g. the one an IWDG reset cut short).
#[must_use]
pub fn previous_used() -> Option<usize> {
    match PREV_USED.load(Ordering::Relaxed) {
        u32::MAX => None,
        n => Some(n as usize),
    }
}

/// Whether the stack lies below the statics (flip-link) rather than above them (default
/// cortex-m-rt layout); reported by `system stack print`.
#[must_use]
pub fn flip_link_layout() -> bool {
    below_statics()
}
