//! WS2812B / SK6812 addressable-LED driver: timer PWM + DMA.
//!
//! Drives a strip on **PA1** (the TOWER WS2812 data pin) using **TIM2 channel 2**
//! in PWM mode: one timer period is one protocol bit (1.25 µs at 800 kHz), and
//! each bit's high-time is set by a per-bit compare value that **DMA1 channel 3**
//! (the TIM2_CH2 request) streams from RAM into the capture/compare register on
//! every timer update. A short compare = logic '0', a long compare = logic '1'.
//! A few zero-duty slots after the data drain the compare preload and leave the
//! line low; the next frame then waits until the line has been low for the latch
//! (reset) time, so a frame returns as soon as it is out.
//!
//! **One byte per bit.** The compare buffer is `u8` (a compare value is at most the
//! ~20-tick period), and the DMA moves it with an 8-bit memory read and a 16-bit
//! peripheral write, which zero-extends each byte into the 16-bit `CCR2`. That halves
//! the RAM of a `u16` buffer: a 144-pixel RGBW strip needs 4.6 KB instead of 9.2 KB,
//! which matters on the 20 KB part. An 8-bit *peripheral* write would not do: the L0
//! timer registers take only 16/32-bit accesses, and the DMA replicates a byte on all
//! bus lanes (`0x0B` would land as `0x0B0B`, a line stuck high). embassy's
//! `SimplePwm::waveform` uses the same size on both sides, so this driver starts the
//! DMA itself (`dma::Channel::write_raw::<u8, u16>`) and only borrows `SimplePwm` for
//! the timer setup.
//!
//! **Timing** fits both the WS2812B and the SK6812 (RGBW) datasheets, ±150 ns each:
//! T0H 0.3 µs (SK6812) / 0.4 µs (WS2812B) and T1H 0.6 / 0.8 µs leave the common
//! windows 0.25 to 0.45 µs for a '0' and 0.65 to 0.75 µs for a '1'. At the 16 MHz
//! sysclk (20 ticks per bit) the driver sends 6 ticks = 0.375 µs and 11 ticks =
//! 0.6875 µs, the middle of each window. The latch wait is [`LATCH_US`] (newer
//! WS2812B revisions need > 280 µs low; SK6812 > 80 µs).
//!
//! Supports 24-bit **RGB** (WS2812B, sent G-R-B) and 32-bit **RGBW** (SK6812,
//! sent G-R-B-W), and an arbitrary pixel count up to the buffer the driver is
//! sized for: `Ws2812::<N>` where `N` = [`rgb_buf_len`] / [`rgbw_buf_len`]. The pixel
//! types are [`tower_strip`]'s (re-exported here).
//!
//! **Encoding** goes a nibble at a time through a 16-entry table of four compare
//! values packed in a word (built for the timer's actual period), so a byte is two
//! aligned word stores instead of eight bit tests.
//!
//! **Cancel safety.** A frame dropped mid-transfer (its `write_*` future cancelled,
//! e.g. by a `select` with a timeout) stops the DMA, but the timer would keep
//! repeating the last bit's compare value forever with no latch gap: a strip fed an
//! endless run of '1' bits lights every channel of every LED at full (about 10 A on
//! 144 RGBW pixels). A drop guard therefore disables the timer's DMA request and
//! parks CCR2 at 0 whenever a transfer ends, cancelled or not, after the DMA has
//! stopped; the strip keeps a partial update at worst, and the next frame still waits
//! out the latch.
//!
//! **Clock requirement:** the timer must resolve those high-times, so it needs a
//! fast clock: comfortable at the firmware's 16 MHz sysclk. It will *not* meet the
//! timing at the old MSI 4 MHz (5 ticks per bit). Compare values are derived from
//! the timer's actual period, so they self-adjust to the clock.
//!
//! ```ignore
//! let mut strip = Ws2812::<{ ws2812::rgb_buf_len(8) }>::new(p.TIM2, p.PA1, p.DMA1_CH3);
//! strip.write_rgb(&[Rgb::new(16, 0, 0); 8]).await;
//! ```

use embassy_stm32::Peri;
use embassy_stm32::bind_interrupts;
use embassy_stm32::dma::{self, InterruptHandler, TransferOptions};
use embassy_stm32::gpio::OutputType;
use embassy_stm32::pac::TIM2 as TIM2_REGS;
use embassy_stm32::pac::timer::vals::Ccds;
use embassy_stm32::peripherals::{DMA1_CH3, PA1, TIM2};
use embassy_stm32::time::Hertz;
use embassy_stm32::timer::low_level::CountingMode;
use embassy_stm32::timer::simple_pwm::{PwmPin, SimplePwm};
use embassy_stm32::timer::{Ch2, Dma};
use embassy_time::{Duration, Instant, Timer};

// DMA1_CH3 (TIM2_CH2 request) shares the L0 DMA1_CHANNEL2_3 vector. Only this
// driver uses that channel, so binding it here is self-contained.
bind_interrupts!(struct Irqs {
    DMA1_CHANNEL2_3 => InterruptHandler<DMA1_CH3>;
});

/// WS2812 bit rate: one timer period per protocol bit.
const BIT_HZ: u32 = 800_000;

/// Zero-duty slots appended after the data. With the compare preload, a value the DMA
/// writes at one update is output from the next, so two slots already flush the last
/// bit and park the line low before the transfer completes; four keep a margin.
pub const TAIL_SLOTS: usize = 4;

/// Reset/latch: how long the line stays low after a frame, in µs, before the next
/// one may start. Covers the newer WS2812B (> 280 µs) and the SK6812 (> 80 µs). It is
/// a timer wait at the start of the next frame, not buffer slots (300 µs of slots would
/// cost 240 bytes of RAM), and it also holds after a cancelled frame.
pub const LATCH_US: u64 = 300;

const RGB_BITS: usize = 24;
const RGBW_BITS: usize = 32;

pub use tower_strip::{Rgb, Rgbw};

/// Compare-buffer length (bytes) needed for `pixels` RGB LEDs.
pub const fn rgb_buf_len(pixels: usize) -> usize {
    pixels * RGB_BITS + TAIL_SLOTS
}

/// Compare-buffer length (bytes) needed for `pixels` RGBW LEDs.
pub const fn rgbw_buf_len(pixels: usize) -> usize {
    pixels * RGBW_BITS + TAIL_SLOTS
}

/// WS2812/SK6812 strip on PA1 (TIM2_CH2 + DMA1_CH3). `N` is the compare-value
/// buffer length in bytes; size it with [`rgb_buf_len`] / [`rgbw_buf_len`].
pub struct Ws2812<'d, const N: usize> {
    // Owned for the timer's lifetime (PA1 + TIM2 stay configured as the PWM output);
    // the per-frame DMA request and transfer go through the registers directly.
    _pwm: SimplePwm<'d, TIM2>,
    dma: Peri<'d, DMA1_CH3>,
    buf: Slots<N>,
    /// The four compare values of each nibble, MSB first (`ccr0` for a '0' bit, `ccr1`
    /// for a '1'), packed little-endian so the first slot is the word's low byte.
    nibbles: [u32; 16],
    /// When the next frame may start: the previous one's end plus the latch.
    ready_at: Instant,
}

/// The compare-value buffer, word-aligned so [`Ws2812::encode_byte`] can store whole words.
#[repr(C, align(4))]
struct Slots<const N: usize>([u8; N]);

/// Ends a transfer, normally or cancelled: no more DMA requests, CCR2 parked at 0 so the
/// line goes low after the bit in progress (see the module docs, "Cancel safety").
struct Park;

impl Drop for Park {
    fn drop(&mut self) {
        TIM2_REGS.dier().modify(|w| w.set_ccde(1, false));
        TIM2_REGS.ccr(1).write(|w| w.set_ccr(0));
    }
}

impl<'d, const N: usize> Ws2812<'d, N> {
    /// Create the driver: `tim` = TIM2, `data` = PA1, `dma` = DMA1_CH3.
    pub fn new(tim: Peri<'d, TIM2>, data: Peri<'d, PA1>, dma: Peri<'d, DMA1_CH3>) -> Self {
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
        // A byte buffer holds compare values up to 255: true for any period this bit rate
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
        Self {
            _pwm: pwm,
            dma,
            buf: Slots([0; N]),
            nibbles,
            ready_at: Instant::MIN,
        }
    }

    /// Send RGB pixels (WS2812B). Awaits the previous frame's latch and the DMA transfer.
    pub async fn write_rgb(&mut self, pixels: &[Rgb]) {
        self.write_rgb_iter(pixels.iter().copied()).await;
    }

    /// Send RGBW pixels (SK6812). Awaits the previous frame's latch and the DMA transfer.
    pub async fn write_rgbw(&mut self, pixels: &[Rgbw]) {
        self.write_rgbw_iter(pixels.iter().copied()).await;
    }

    /// [`write_rgb`](Self::write_rgb) from an iterator, so a caller can encode straight
    /// from its own framebuffer (applying brightness, ...) without a second pixel array.
    pub async fn write_rgb_iter(&mut self, pixels: impl IntoIterator<Item = Rgb>) {
        let mut i = 0;
        for px in pixels {
            assert!(
                i + RGB_BITS + TAIL_SLOTS <= N,
                "ws2812: buffer too small for RGB pixels"
            );
            i = self.encode_byte(i, px.g);
            i = self.encode_byte(i, px.r);
            i = self.encode_byte(i, px.b);
        }
        self.flush(i).await;
    }

    /// [`write_rgbw`](Self::write_rgbw) from an iterator (see
    /// [`write_rgb_iter`](Self::write_rgb_iter)).
    pub async fn write_rgbw_iter(&mut self, pixels: impl IntoIterator<Item = Rgbw>) {
        let mut i = 0;
        for px in pixels {
            assert!(
                i + RGBW_BITS + TAIL_SLOTS <= N,
                "ws2812: buffer too small for RGBW pixels"
            );
            i = self.encode_byte(i, px.g);
            i = self.encode_byte(i, px.r);
            i = self.encode_byte(i, px.b);
            i = self.encode_byte(i, px.w);
        }
        self.flush(i).await;
    }

    /// Encode one byte MSB-first into `buf` from index `i`; return the next index.
    fn encode_byte(&mut self, i: usize, byte: u8) -> usize {
        assert!(i.is_multiple_of(8) && i + 8 <= N, "ws2812: encode out of bounds");
        let (hi, lo) = (
            self.nibbles[(byte >> 4) as usize],
            self.nibbles[(byte & 0x0F) as usize],
        );
        // SAFETY: `i` is a multiple of 8 and `i + 8 <= N` (asserted above), and the buffer is
        // 4-aligned (`Slots`), so both words are aligned and inside it.
        unsafe {
            let word = self.buf.0.as_mut_ptr().add(i) as *mut u32;
            word.write(hi);
            word.add(1).write(lo);
        }
        i + 8
    }

    async fn flush(&mut self, data_end: usize) {
        let used = data_end + TAIL_SLOTS;
        self.buf.0[data_end..used].fill(0);

        // The previous frame's latch: the line has been low since it ended (or was cancelled).
        Timer::at(self.ready_at).await;
        // If this frame is cancelled, the next one must still not start within a latch of
        // wherever it stopped: assume the whole frame went out (conservative), and correct it
        // below once it really has.
        self.ready_at = Instant::now() + Duration::from_micros(used as u64 * 5 / 4 + LATCH_US);

        let request = Dma::<TIM2, Ch2>::request(&*self.dma);
        let ccr2 = TIM2_REGS.ccr(1).as_ptr() as *mut u16;
        let mut channel = dma::Channel::new(self.dma.reborrow(), Irqs);
        // Created BEFORE the transfer, so on a cancel the transfer drops first (the DMA channel
        // is stopped) and `park` after it: with the channel stopped, nothing can rewrite CCR2
        // once `park` zeroes it. (The other order would race: a request already in flight when
        // CC2DE is cleared could land after the zero and leave a data value repeating.)
        let park = Park;
        // SAFETY: `buf` and the channel stay borrowed until the transfer completes (it is
        // awaited here, and dropping the future stops the DMA before the borrow ends);
        // `ccr2` is TIM2's CCR2, a 16-bit write target for the whole transfer.
        let transfer = unsafe {
            channel.write_raw::<u8, u16>(
                request,
                &self.buf.0[..used] as *const [u8],
                ccr2,
                TransferOptions::default(),
            )
        };
        // Arm the timer's request only after the channel is armed: the first slot then
        // lands in the CCR2 preload at the next update, on a bit boundary.
        TIM2_REGS.dier().modify(|w| w.set_ccde(1, true));
        transfer.await;
        drop(park);

        // The tail slots left CCR2 = 0 (line low); the next frame waits out the latch from
        // here (see above), so this one returns without holding the caller for it.
        self.ready_at = Instant::now() + Duration::from_micros(LATCH_US);
    }
}
