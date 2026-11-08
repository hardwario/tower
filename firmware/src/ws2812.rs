//! WS2812B / SK6812 addressable-LED driver: timer PWM + a streamed DMA ring.
//!
//! Drives a strip on **PA1** (the TOWER WS2812 data pin) using **TIM2 channel 2**
//! in PWM mode: one timer period is one protocol bit (1.25 µs at 800 kHz), and
//! each bit's high-time is set by a per-bit compare value that **DMA1 channel 3**
//! (the TIM2_CH2 request) writes into the capture/compare register on every timer
//! update. A short compare = logic '0', a long compare = logic '1'.
//!
//! **Streaming.** One compare value per bit is 32 bytes per RGBW pixel: a whole
//! 144-pixel frame encoded up front would take 4.6 KB of the 20 KB RAM, too much
//! next to the radio stack. So a frame is kept as its wire *levels* (one byte per
//! channel, 4 bytes per RGBW pixel) and encoded just in time into a small ring of
//! compare values ([`RING_SLOTS`] = 256 bytes, two halves of [`HALF_BYTES`] levels =
//! 160 µs each) that the DMA plays in circular mode. When a half has been played,
//! the DMA's half-transfer or transfer-complete interrupt refills it with the next
//! 16 levels while the DMA plays the other half: a deadline of 160 µs for about
//! 10 µs of work. The interrupt runs at the highest priority and is a plain handler,
//! not an async task, so no busy task can delay it (embassy's executor is
//! cooperative). After the last level it fills zeros, and once a whole zero half
//! has been played it stops the DMA with the line low.
//!
//! **Underruns.** A refill that comes too late lets the DMA replay stale compare
//! values, which shows as wrong colours for the rest of that frame. The interrupt
//! checks where the DMA is before and after each refill and counts any late one in
//! [`underruns`]; it should stay at zero.
//!
//! **EEPROM writes cut the frame.** Programming the data EEPROM stalls instruction
//! fetch from flash for milliseconds, so the refill interrupt (code in flash) cannot run
//! and the DMA would replay stale bits: measured on the bench, about one underrun per
//! EEPROM write. The storage therefore calls [`yield_to_nvm`] just before it programs,
//! which stops an active stream cleanly with the line low: the pixels past the cut keep
//! the previous frame's colours until the next frame (a one-frame lag on part of the
//! strip, not garbage), and the EEPROM write, which the radio's counters and replay lanes
//! depend on, never waits for the strip. Counted in [`cuts`]. Background work that can
//! wait (the KV store's maintenance slices) checks [`streaming`] first and runs between
//! frames instead, so it cuts none.
//!
//! **One byte per bit, 16-bit writes.** The ring is `u8` (a compare value is at most
//! the ~20-tick period), and the DMA moves it with an 8-bit memory read and a 16-bit
//! peripheral write, which zero-extends each byte into the 16-bit `CCR2`. An 8-bit
//! *peripheral* write would not do: the L0 timer registers take only 16/32-bit
//! accesses, and the DMA replicates a byte on all bus lanes (`0x0B` would land as
//! `0x0B0B`, a line stuck high). The DMA channel is programmed directly (embassy's
//! `SimplePwm` sets up the timer, nothing more).
//!
//! **Timing** fits both the WS2812B and the SK6812 (RGBW) datasheets, ±150 ns each:
//! T0H 0.3 µs (SK6812) / 0.4 µs (WS2812B) and T1H 0.6 / 0.8 µs leave the common
//! windows 0.25 to 0.45 µs for a '0' and 0.65 to 0.75 µs for a '1'. At the 16 MHz
//! sysclk (20 ticks per bit) the driver sends 6 ticks = 0.375 µs and 11 ticks =
//! 0.6875 µs, the middle of each window. The next frame waits until the line has been
//! low for [`LATCH_US`] (newer WS2812B revisions need > 280 µs; SK6812 > 80 µs), so a
//! frame returns as soon as it is out.
//!
//! Supports 24-bit **RGB** (WS2812B, sent G-R-B) and 32-bit **RGBW** (SK6812,
//! sent G-R-B-W), and an arbitrary pixel count up to the levels buffer the driver is
//! sized for: `Ws2812::<N>` where `N` = [`rgb_buf_len`] / [`rgbw_buf_len`]. The pixel
//! types are [`tower_strip`]'s (re-exported here).
//!
//! **Encoding** goes a nibble at a time through a 16-entry table of four compare
//! values packed in a word (built for the timer's actual period), so a level byte is
//! two aligned word stores.
//!
//! **Cancel safety.** A frame dropped mid-stream (its `write_*` future cancelled,
//! e.g. by a `select` with a timeout) must not leave the timer repeating the last
//! bit's compare value forever with no latch gap: a strip fed an endless run of '1'
//! bits lights every channel of every LED at full (about 10 A on 144 RGBW pixels).
//! A drop guard therefore stops the stream whenever a frame ends, cancelled or not:
//! the interrupt stands down, the DMA channel stops, the timer's DMA request goes off
//! and CCR2 is parked at 0. The strip keeps a partial update at worst, and the next
//! frame still waits out the latch.
//!
//! **One strip.** The ring, the stream state and the `DMA1_CHANNEL2_3` interrupt are
//! the driver's own (statics: there is one PA1, one TIM2 and one DMA1 channel 3), so
//! nothing else may bind that interrupt (DMA1 channels 2 and 3 are otherwise unused). The
//! app binds it to [`Refill`] (`bind_interrupts!`), so the driver's state is linked only
//! into apps that drive a strip.
//!
//! **Clock requirement:** the timer must resolve those high-times, so it needs a
//! fast clock: comfortable at the firmware's 16 MHz sysclk. It will *not* meet the
//! timing at the old MSI 4 MHz (5 ticks per bit). Compare values are derived from
//! the timer's actual period, so they self-adjust to the clock.
//!
//! ```ignore
//! bind_interrupts!(struct Irqs { DMA1_CHANNEL2_3 => ws2812::Refill; });
//! let mut strip = Ws2812::<{ ws2812::rgb_buf_len(8) }>::new(p.TIM2, p.PA1, p.DMA1_CH3, Irqs);
//! strip.write_rgb(&[Rgb::new(16, 0, 0); 8]).await;
//! ```

use core::cell::UnsafeCell;
use core::future::poll_fn;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering::Relaxed};
use core::task::Poll;

use embassy_stm32::Peri;
use embassy_stm32::gpio::OutputType;
use embassy_stm32::interrupt::{self, InterruptExt, Priority, typelevel};
use embassy_stm32::pac::DMA1;
use embassy_stm32::pac::TIM2 as TIM2_REGS;
use embassy_stm32::pac::bdma::vals::{Dir, Pl, Size};
use embassy_stm32::pac::timer::vals::Ccds;
use embassy_stm32::peripherals::{DMA1_CH3, PA1, TIM2};
use embassy_stm32::rcc::{StopMode, WakeGuard};
use embassy_stm32::time::Hertz;
use embassy_stm32::timer::low_level::CountingMode;
use embassy_stm32::timer::simple_pwm::{PwmPin, SimplePwm};
use embassy_stm32::timer::{Ch2, Dma};
use embassy_sync::waitqueue::AtomicWaker;
use embassy_time::{Duration, Instant, Timer};

pub use tower_strip::{Rgb, Rgbw};

/// WS2812 bit rate: one timer period per protocol bit.
const BIT_HZ: u32 = 800_000;

/// Levels (bytes of pixel data) per ring half: 16 levels = 128 bits = 160 µs, the
/// interrupt's deadline for refilling a half.
pub const HALF_BYTES: usize = 16;
const HALF_SLOTS: usize = HALF_BYTES * 8;
/// The ring of compare values the DMA plays: two halves.
pub const RING_SLOTS: usize = 2 * HALF_SLOTS;

/// Reset/latch: how long the line stays low after a frame, in µs, before the next
/// one may start. Covers the newer WS2812B (> 280 µs) and the SK6812 (> 80 µs). It is
/// a timer wait at the start of the next frame, and it also holds after a cancelled
/// frame.
pub const LATCH_US: u64 = 300;

/// DMA1 channel 3, as the register block numbers it (from 0).
const CH: usize = 2;

/// Levels buffer length (bytes) for `pixels` RGB LEDs.
pub const fn rgb_buf_len(pixels: usize) -> usize {
    pixels * 3
}

/// Levels buffer length (bytes) for `pixels` RGBW LEDs.
pub const fn rgbw_buf_len(pixels: usize) -> usize {
    pixels * 4
}

/// Late refills since boot (all frames); see the module docs, "Underruns".
pub fn underruns() -> u32 {
    STREAM.underruns.load(Relaxed)
}

/// Frames cut short for an EEPROM write since boot; see the module docs.
pub fn cuts() -> u32 {
    STREAM.cuts.load(Relaxed)
}

/// Whether a frame is streaming now, from the hardware alone: DMA1 channel 3 running with
/// TIM2's CC2 DMA request on (only this driver sets that request). Background EEPROM work
/// (the KV maintenance task) waits for this to clear, so it runs between frames instead of
/// cutting one.
///
/// It and [`yield_to_nvm`] read registers, not the driver's state, on purpose: the storage
/// calls them in every app, and a reference to the stream state would link its ~350 bytes of
/// RAM into apps without a strip too (some sit at their stack floor).
pub fn streaming() -> bool {
    DMA1.ch(CH).cr().read().en() && TIM2_REGS.dier().read().ccde(1)
}

/// Cut an active stream before the EEPROM is programmed (the storage calls this; see the
/// module docs, "EEPROM writes cut the frame"): the DMA stops and the line parks low here,
/// and the strip interrupt is pended so the driver, finding its channel stopped, ends the
/// frame and wakes `show`. A no-op without a streaming strip.
pub fn yield_to_nvm() {
    if streaming() {
        DMA1.ch(CH).cr().modify(|w| w.set_en(false));
        TIM2_REGS.dier().modify(|w| w.set_ccde(1, false));
        TIM2_REGS.ccr(1).write(|w| w.set_ccr(0));
        // Runs at once (it preempts this thread), so before the EEPROM write starts.
        interrupt::DMA1_CHANNEL2_3.pend();
    }
}

/// The ring, word-aligned so a refill can store whole words.
#[repr(C, align(4))]
struct Ring([u8; RING_SLOTS]);

/// What the interrupt and the thread share. The thread writes the ring, the source and
/// the half flags only while the stream is stopped (`active` false); while it runs, only
/// the interrupt touches them. The fields that cross are atomics (load/store only: the M0+
/// has no read-modify-write).
struct Stream {
    ring: UnsafeCell<Ring>,
    /// The nibble table: the four compare values of each nibble, MSB first, packed
    /// little-endian (the first slot is the word's low byte). Written once by `new`.
    nibbles: UnsafeCell<[u32; 16]>,
    /// The frame's levels, `len` bytes, and how many the ring has taken so far.
    src: AtomicPtr<u8>,
    len: AtomicUsize,
    pos: AtomicUsize,
    /// Whether ring half 0 / 1 holds only zeros (no data left when it was filled).
    zero: [AtomicBool; 2],
    active: AtomicBool,
    done: AtomicBool,
    underruns: AtomicU32,
    cuts: AtomicU32,
    waker: AtomicWaker,
}

// SAFETY: the `UnsafeCell`s are accessed by one side at a time, as documented on `Stream`.
unsafe impl Sync for Stream {}

static STREAM: Stream = Stream {
    ring: UnsafeCell::new(Ring([0; RING_SLOTS])),
    nibbles: UnsafeCell::new([0; 16]),
    src: AtomicPtr::new(core::ptr::null_mut()),
    len: AtomicUsize::new(0),
    pos: AtomicUsize::new(0),
    zero: [AtomicBool::new(true), AtomicBool::new(true)],
    active: AtomicBool::new(false),
    done: AtomicBool::new(true),
    underruns: AtomicU32::new(0),
    cuts: AtomicU32::new(0),
    waker: AtomicWaker::new(),
};

/// Encode the next levels into ring half `h`, zero-padding past the end of the frame.
/// Returns whether the half is all zeros (no data was left).
///
/// # Safety
/// Only one side at a time (see [`Stream`]); `src` must point at `len` readable bytes.
unsafe fn fill_half(h: usize) -> bool {
    let len = STREAM.len.load(Relaxed);
    let pos = STREAM.pos.load(Relaxed);
    let src = STREAM.src.load(Relaxed);
    let n = len.saturating_sub(pos).min(HALF_BYTES);
    // SAFETY (caller): exclusive access to the ring and the table; the half is inside the
    // 4-aligned ring and `HALF_SLOTS` is a multiple of 8, so every word store is aligned.
    unsafe {
        let nibbles = &*STREAM.nibbles.get();
        let words = ((*STREAM.ring.get()).0.as_mut_ptr() as *mut u32).add(h * HALF_SLOTS / 4);
        for k in 0..n {
            let b = *src.add(pos + k);
            words.add(2 * k).write(nibbles[(b >> 4) as usize]);
            words.add(2 * k + 1).write(nibbles[(b & 0x0F) as usize]);
        }
        for k in n..HALF_BYTES {
            words.add(2 * k).write(0);
            words.add(2 * k + 1).write(0);
        }
    }
    STREAM.pos.store(pos + n, Relaxed);
    n == 0
}

/// Stop the stream: the interrupt stands down, the DMA channel stops, no more timer DMA
/// requests, CCR2 parked at 0 (the line goes low after the bit in progress). Idempotent.
/// The channel stops before CCR2 is parked, so nothing can rewrite it afterwards.
fn stop() {
    STREAM.active.store(false, Relaxed);
    DMA1.ch(CH).cr().modify(|w| w.set_en(false));
    TIM2_REGS.dier().modify(|w| w.set_ccde(1, false));
    TIM2_REGS.ccr(1).write(|w| w.set_ccr(0));
    STREAM.done.store(true, Relaxed);
    STREAM.waker.wake();
}

/// Whether the DMA is in ring half 0 now (from its remaining count: it counts down from
/// `RING_SLOTS` and reloads at the wrap).
fn dma_in_half0() -> bool {
    DMA1.ch(CH).ndtr().read().ndt() as usize > HALF_SLOTS
}

/// The `DMA1_CHANNEL2_3` interrupt: refill the half the DMA has just played. Bind it in the
/// app that drives a strip and pass the binding to [`Ws2812::new`] (or `Strip::new`):
///
/// ```ignore
/// bind_interrupts!(struct Irqs { DMA1_CHANNEL2_3 => tower::ws2812::Refill; });
/// ```
///
/// (The binding lives in the app, not the SDK, so apps without a strip link none of the
/// driver's state.)
pub struct Refill;

impl typelevel::Handler<typelevel::DMA1_CHANNEL2_3> for Refill {
    unsafe fn on_interrupt() {
        let isr = DMA1.isr().read();
        DMA1.ifcr().write(|w| w.set_gif(CH, true)); // clears HT, TC and TE of the channel
        if !STREAM.active.load(Relaxed) {
            return;
        }
        if !DMA1.ch(CH).cr().read().en() {
            // Active but stopped: `yield_to_nvm` cut the frame for an EEPROM write.
            STREAM.cuts.store(STREAM.cuts.load(Relaxed) + 1, Relaxed);
            stop();
            return;
        }
        if isr.teif(CH) {
            STREAM
                .underruns
                .store(STREAM.underruns.load(Relaxed) + 1, Relaxed);
            stop();
            return;
        }
        let (ht, tc) = (isr.htif(CH), isr.tcif(CH));
        if ht && tc {
            // More than half a ring late: a whole half was replayed.
            STREAM
                .underruns
                .store(STREAM.underruns.load(Relaxed) + 1, Relaxed);
        }
        for (h, played) in [(0, ht), (1, tc)] {
            if !played {
                continue;
            }
            if STREAM.zero[h].load(Relaxed) {
                // A whole zero half has been played: the last bit is out and the line low.
                stop();
                return;
            }
            // SAFETY: the stream is active, so the interrupt owns the ring and the source.
            let zero = unsafe { fill_half(h) };
            STREAM.zero[h].store(zero, Relaxed);
            // While refilling half h the DMA must still be in the other half.
            if dma_in_half0() == (h == 0) {
                STREAM
                    .underruns
                    .store(STREAM.underruns.load(Relaxed) + 1, Relaxed);
            }
        }
    }
}

/// Stops the stream when a frame ends, normally or cancelled (see "Cancel safety").
struct StopGuard;

impl Drop for StopGuard {
    fn drop(&mut self) {
        stop();
    }
}

/// WS2812/SK6812 strip on PA1 (TIM2_CH2 + DMA1_CH3). `N` is the levels buffer length in
/// bytes; size it with [`rgb_buf_len`] / [`rgbw_buf_len`].
pub struct Ws2812<'d, const N: usize> {
    // Owned for the timer's lifetime (PA1 + TIM2 stay configured as the PWM output);
    // the per-frame stream goes through the registers directly.
    _pwm: SimplePwm<'d, TIM2>,
    // Owned so nothing else can claim DMA1 channel 3.
    _dma: Peri<'d, DMA1_CH3>,
    /// The DMA request number of TIM2_CH2 on channel 3 (for the channel selection).
    request: u8,
    levels: [u8; N],
    /// When the next frame may start: the previous one's end plus the latch.
    ready_at: Instant,
}

impl<'d, const N: usize> Ws2812<'d, N> {
    /// Create the driver: `tim` = TIM2, `data` = PA1, `dma` = DMA1_CH3, `irq` = the app's
    /// binding of `DMA1_CHANNEL2_3` to [`Refill`].
    pub fn new(
        tim: Peri<'d, TIM2>,
        data: Peri<'d, PA1>,
        dma: Peri<'d, DMA1_CH3>,
        _irq: impl interrupt::typelevel::Binding<typelevel::DMA1_CHANNEL2_3, Refill> + 'd,
    ) -> Self {
        let mut pwm = SimplePwm::new(
            tim,
            None,
            Some(PwmPin::new(data, OutputType::PushPull)), // CH2 = PA1
            None,
            None,
            Hertz(BIT_HZ),
            CountingMode::EdgeAlignedUp,
        );
        // Period = `max` ticks. High-time as a fraction of the 1.25 µs bit:
        // '0' = 0.30 (0.375 µs), '1' = 0.56 (0.70 µs), rounded to the nearest tick;
        // 6 and 11 ticks at 16 MHz (see the module docs for the two datasheets).
        let max = pwm.max_duty_cycle();
        let ccr0 = (max * 30 + 50) / 100;
        let ccr1 = (max * 56 + 50) / 100;
        // A byte ring holds compare values up to 255: true for any period this bit rate
        // gets from a sysclk up to 200 MHz, so it only fails on a misconfigured clock.
        assert!(
            ccr1 <= u8::MAX as u32,
            "ws2812: timer clock too fast for u8 compare values"
        );

        // Line low (0 % duty) between frames; output enabled for good. (This channel
        // handle owns no pin, so dropping it leaves PA1 and the output as they are.)
        {
            let mut ch = pwm.ch2();
            ch.set_duty_cycle_fully_off();
            ch.enable();
        }
        // CC2's DMA request fires on the update event (one per bit period), not on the
        // compare match; it is only *enabled* (CC2DE) for the length of a frame.
        TIM2_REGS.cr2().modify(|w| w.set_ccds(Ccds::ON_UPDATE));

        let mut nibbles = [0u32; 16];
        for (n, word) in nibbles.iter_mut().enumerate() {
            let mut slots = [0u8; 4];
            for (bit, slot) in slots.iter_mut().enumerate() {
                *slot = if n & (8 >> bit) != 0 {
                    ccr1 as u8
                } else {
                    ccr0 as u8
                };
            }
            *word = u32::from_le_bytes(slots);
        }
        stop(); // in case a previous driver left a stream behind
        // SAFETY: the stream is stopped, so nothing else reads the table now.
        unsafe { *STREAM.nibbles.get() = nibbles };

        let request = Dma::<TIM2, Ch2>::request(&*dma);
        // The refill must beat the DMA by half a ring: the highest interrupt priority.
        interrupt::DMA1_CHANNEL2_3.set_priority(Priority::P0);
        // SAFETY: `Refill` is bound to this vector and only acts while a stream is active.
        unsafe { interrupt::DMA1_CHANNEL2_3.enable() };

        Self {
            _pwm: pwm,
            _dma: dma,
            request,
            levels: [0; N],
            ready_at: Instant::MIN,
        }
    }

    /// Send RGB pixels (WS2812B). Awaits the previous frame's latch and the stream.
    pub async fn write_rgb(&mut self, pixels: &[Rgb]) {
        self.write_rgb_iter(pixels.iter().copied()).await;
    }

    /// Send RGBW pixels (SK6812). Awaits the previous frame's latch and the stream.
    pub async fn write_rgbw(&mut self, pixels: &[Rgbw]) {
        self.write_rgbw_iter(pixels.iter().copied()).await;
    }

    /// [`write_rgb`](Self::write_rgb) from an iterator, so a caller can render straight
    /// from its own framebuffer (applying brightness, ...) without a second pixel array.
    pub async fn write_rgb_iter(&mut self, pixels: impl IntoIterator<Item = Rgb>) {
        let mut i = 0;
        for px in pixels {
            assert!(i + 3 <= N, "ws2812: buffer too small for RGB pixels");
            self.levels[i..i + 3].copy_from_slice(&[px.g, px.r, px.b]);
            i += 3;
        }
        self.send(i).await;
    }

    /// [`write_rgbw`](Self::write_rgbw) from an iterator (see
    /// [`write_rgb_iter`](Self::write_rgb_iter)).
    pub async fn write_rgbw_iter(&mut self, pixels: impl IntoIterator<Item = Rgbw>) {
        let mut i = 0;
        for px in pixels {
            assert!(i + 4 <= N, "ws2812: buffer too small for RGBW pixels");
            self.levels[i..i + 4].copy_from_slice(&[px.g, px.r, px.b, px.w]);
            i += 4;
        }
        self.send(i).await;
    }

    /// Stream the first `len` levels, then return with the line low.
    async fn send(&mut self, len: usize) {
        // The previous frame's latch: the line has been low since it ended (or was cancelled).
        Timer::at(self.ready_at).await;
        // If this frame is cancelled, the next one must still not start within a latch of
        // wherever it stopped: assume the whole frame went out (conservative), and correct it
        // below once it really has. (8 slots a level, 1.25 µs a slot, plus a zero half.)
        let frame_us = ((len + 2 * HALF_BYTES) * 10) as u64;
        self.ready_at = Instant::now() + Duration::from_micros(frame_us + LATCH_US);

        // Load the stream (stopped, so the thread owns it) and pre-fill both halves.
        STREAM.src.store(self.levels.as_mut_ptr(), Relaxed);
        STREAM.len.store(len, Relaxed);
        STREAM.pos.store(0, Relaxed);
        STREAM.done.store(false, Relaxed);
        for h in 0..2 {
            // SAFETY: stopped stream, and `levels` holds `len` bytes for the whole stream:
            // `send` keeps `self` borrowed until `guard` below has stopped it.
            let zero = unsafe { fill_half(h) };
            STREAM.zero[h].store(zero, Relaxed);
        }
        // Stops the stream on every way out, a cancel included, before `levels` is free.
        let guard = StopGuard;
        // No STOP mode while the DMA and the timer run (STOP halts their clocks).
        let wake = WakeGuard::new(StopMode::Stop1);

        let ch = DMA1.ch(CH);
        ch.cr().write(|w| w.set_en(false));
        ch.par().write_value(TIM2_REGS.ccr(1).as_ptr() as u32);
        // SAFETY: the ring is a static; only its address is taken here.
        ch.mar()
            .write_value(unsafe { (*STREAM.ring.get()).0.as_ptr() } as u32);
        ch.ndtr().write(|w| w.set_ndt(RING_SLOTS as u16));
        // CSELR is shared with the other channels' drivers: change ours atomically.
        critical_section::with(|_| DMA1.cselr().modify(|w| w.set_cs(CH, self.request)));
        DMA1.ifcr().write(|w| w.set_gif(CH, true));
        STREAM.active.store(true, Relaxed);
        ch.cr().write(|w| {
            w.set_dir(Dir::FROM_MEMORY);
            w.set_minc(true);
            w.set_circ(true);
            w.set_msize(Size::BITS8);
            w.set_psize(Size::BITS16);
            w.set_pl(Pl::VERY_HIGH);
            w.set_htie(true);
            w.set_tcie(true);
            w.set_teie(true);
            w.set_en(true);
        });
        // Arm the timer's request only after the channel is armed: the first slot then
        // lands in the CCR2 preload at the next update, on a bit boundary.
        TIM2_REGS.dier().modify(|w| w.set_ccde(1, true));

        poll_fn(|cx| {
            STREAM.waker.register(cx.waker());
            if STREAM.done.load(Relaxed) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        drop(guard);
        drop(wake);

        // The line has been low since the stream stopped; the next frame waits out the latch
        // from here (see above), so this one returns without holding the caller for it.
        self.ready_at = Instant::now() + Duration::from_micros(LATCH_US);
    }
}
