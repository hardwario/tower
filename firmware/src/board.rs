//! HARDWARIO TOWER Core Module board support.
//!
//! Most apps use [`Board::take`] via the [`app!`](crate::app) macro and never touch the
//! lower-level [`init`]. `take` performs the *always on* setup: clocks, the USB-gated
//! serial [`console`](crate::console), and putting the TMP112 into shutdown (one-shot)
//! mode so it does not free-run and waste power. It then hands the app a [`Board`] of
//! ready-to-use resources.

use embassy_executor::Spawner;
use embassy_stm32::exti::{ExtiInput, InterruptHandler};
use embassy_stm32::flash::Flash;
use embassy_stm32::gpio::{Flex, Level, Output, Pull, Speed};
use embassy_stm32::i2c::{Config as I2cConfig, I2c, Master};
use embassy_stm32::mode::{Async, Blocking};
use embassy_stm32::peripherals::{
    DMA1_CH3, IWDG, PA1, PA15, PB3, PB4, PB5, PB7, PB10, PB11, PH1, SPI1, TIM2,
};
use embassy_stm32::rcc::{LsConfig, Sysclk};
use embassy_stm32::time::Hertz;
use embassy_stm32::{Peri, Peripherals, bind_interrupts, interrupt};
use embassy_time::Duration;
use embedded_hal::i2c::{ErrorType, I2c as I2cTrait, Operation};
use log::LevelFilter;

use crate::storage::{Nv, Storage};
use crate::tmp112::{self, Tmp112};

// PA8 (button), PA12 (VBUS_SENSE) and PA7 (SPIRIT1 nIRQ) are EXTI lines 8/12/7,
// all on EXTI4_15, no line collision (PB6 accel INT1 is line 6, also here).
bind_interrupts!(struct Irqs {
    EXTI4_15 => InterruptHandler<interrupt::typelevel::EXTI4_15>;
});
// The USART1 interrupt (console UART) is bound in `console` (`ConsoleIrqs`), which owns
// the UART and rebuilds it dynamically as USB is plugged/unplugged.

/// Default console verbosity set by [`Board::take`]; lower it at runtime with
/// `log::set_max_level`.
const LOG_LEVEL: LevelFilter = LevelFilter::Trace;

// I2C2 addresses of the always-on bus devices `Board::take` quiesces at boot.
const ACCEL_ADDR: u8 = 0x19; // LIS2DH12 accelerometer (SA0 tied high)
const LIS2DH12_CTRL_REG1: u8 = 0x20; // ODR/enable; 0x00 = power-down
const ATSHA_ADDR: u8 = 0x64; // ATSHA204A crypto (word address 0x01 = Sleep)

/// The board's shared I²C2 bus (blocking), wrapped in an [`AtshaGuard`] so every
/// transaction automatically re-sleeps the ATSHA204A. This is the bus type the app
/// receives (via [`Board::tmp112`] and `release()`).
pub type GuardedI2c = AtshaGuard<I2c<'static, Blocking, Master>>;

/// The board's TMP112 on the (ATSHA-guarded) I²C2 bus (blocking), in shutdown / one-shot mode.
pub type Tmp112Sensor = Tmp112<GuardedI2c>;

/// Ready-to-use board resources handed to an app by [`Board::take`].
///
/// The console is already running and the [`tmp112`](Board::tmp112) sensor is in
/// shutdown mode (call `oneshot()` to read). The remaining fields are the
/// board's user peripherals; build the blocks you need from them.
pub struct Board {
    /// Task spawner for the running executor.
    pub spawner: Spawner,
    /// TMP112 temperature sensor, shut down and ready for `oneshot()` reads.
    pub tmp112: Tmp112Sensor,
    /// On-board LED pin (PH1, active-high); pass to [`led::init`](crate::led::init).
    pub led: Peri<'static, PH1>,
    /// On-board button (PA8, active-high, pull-down), EXTI-bound; pass to
    /// [`button::init_exti`](crate::button::init_exti) with [`Polarity::ActiveHigh`](crate::button::Polarity::ActiveHigh).
    pub button: ExtiInput<'static, Async>,
    /// WS2812 strip timer (TIM2); pass to [`strip::Strip::new`](crate::strip::Strip::new).
    pub strip_tim: Peri<'static, TIM2>,
    /// WS2812 strip data pin (PA1).
    pub strip_data: Peri<'static, PA1>,
    /// WS2812 strip DMA channel (DMA1_CH3).
    pub strip_dma: Peri<'static, DMA1_CH3>,
    /// LIS2DH12 accelerometer INT1 line (PB6, active-high, pull-down), EXTI-bound,
    /// for the tilt/movement interrupt. The accelerometer shares the I²C2 bus
    /// with the TMP112; reclaim it with [`tmp112`](Self::tmp112)`.release()`.
    pub accel_int: ExtiInput<'static, Async>,
    /// The one shared key-value store over the data EEPROM; see [`storage`](crate::storage).
    /// `Copy`; hand the same handle to `Net`, the [`shell`](crate::shell), and the app at once
    /// (each call locks the one store).
    pub kv: Nv,
    /// Independent watchdog peripheral (**opt-in**). Pass to
    /// [`watchdog::enable`](crate::watchdog::enable) to arm a hardware reset on a firmware hang;
    /// apps that don't want it simply ignore this field. The IWDG runs from the LSI and keeps
    /// counting in STOP, so the feeder must pet it during idle (see the `watchdog` module).
    pub iwdg: Peri<'static, IWDG>,

    // --- SPIRIT1 sub-GHz radio (SPSGRF module); see [`radio`](crate::radio). ---
    /// SPIRIT1 shutdown pin (PB7). Has a 1 MΩ hardware pull-up so the part boots
    /// into SHUTDOWN; the driver must drive it **low** to enable the radio.
    pub radio_sdn: Peri<'static, PB7>,
    /// SPIRIT1 SPI chip-select (PA15): **software-controlled** (≥2 µs setup), so
    /// the radio driver owns it as a GPIO output, not the SPI peripheral's NSS.
    pub radio_cs: Peri<'static, PA15>,
    /// SPI1 peripheral for the radio bus (≤10 MHz, mode 0). Blocking; see the
    /// `"spi"` feature note in Cargo.toml.
    pub radio_spi: Peri<'static, SPI1>,
    /// SPI1 SCLK (PB3).
    pub radio_sck: Peri<'static, PB3>,
    /// SPI1 MOSI (PB5).
    pub radio_mosi: Peri<'static, PB5>,
    /// SPI1 MISO (PB4).
    pub radio_miso: Peri<'static, PB4>,
    /// SPIRIT1 GPIO0 / nIRQ (PA7, active-low), EXTI line 7 (on the bound
    /// `EXTI4_15` group); it wakes the driver on radio events. Pulled up so the
    /// idle (de-asserted) level is defined before the SPIRIT1 drives it.
    pub radio_irq: ExtiInput<'static, Async>,
}

/// Boot indicator timings: the common HARDWARIO TOWER power-on signature every
/// board plays before its application starts (driven by [`Board::boot_indicator`],
/// called from the [`app!`](crate::app) macro). Total 3 s.
const BOOT_SETTLE_MS: u64 = 500; // do nothing first; let the supply settle (a
// battery can bounce on its contacts at insertion), so a brown-out during this
// window re-runs cleanly rather than mid-application.
const BOOT_ON_MS: u64 = 2000; // LED solid on: "powered, booting".
const BOOT_TAIL_MS: u64 = 500; // LED off: a clear gap before the app takes the LED.

impl Board {
    /// Play the common TOWER power-on LED signature on the board LED (PH1,
    /// active-high): **500 ms off → 2 s on → 500 ms off**, then return. Called by
    /// the [`app!`](crate::app) macro after the SDK is up and before the app's
    /// `run`, so every board (whatever the app) shows the same 3 s boot
    /// indication. Borrows the LED pin for the duration and hands it back, so the
    /// app still owns `board.led` for its own [`led::init`](crate::led::init).
    ///
    /// Async, not blocking: the awaits let the console come up and drain the boot
    /// backlog during the signature (the `Hello`/boot logs are emitted meanwhile),
    /// so only the *application* is delayed by the 3 s, not the host link.
    pub async fn boot_indicator(&mut self) {
        let mut led = Output::new(self.led.reborrow(), Level::Low, Speed::Low);
        embassy_time::Timer::after_millis(BOOT_SETTLE_MS).await;
        led.set_high();
        embassy_time::Timer::after_millis(BOOT_ON_MS).await;
        led.set_low();
        embassy_time::Timer::after_millis(BOOT_TAIL_MS).await;
        // `led` drops here → the pin is released back to `self.led` for the app.
    }

    /// Initialise the board and return its resources. Call once at start-up,
    /// typically via [`app!`](crate::app). Performs the always-on setup: clocks,
    /// the serial console, TMP112 shutdown, and **USB-aware power management**
    /// (see below).
    ///
    /// The USB-presence-gated [`console::manager`](crate::console::manager) is spawned
    /// automatically, so every app gets the same low-power policy without wiring it up:
    /// the console UART is up (and interrupts responsive) while USB is connected, and is
    /// torn down when unplugged so the executor can reach STOP and idle at µA.
    pub fn take(spawner: Spawner) -> Self {
        let mut p = init();

        // Dynamic, USB-presence-gated console. On the STM32L0 an *enabled* USART holds
        // embassy's STOP refcount, so a permanently-on console would keep the low-power
        // executor out of STOP forever: an unplugged node would burn ~3.5 mA (WFI at
        // 16 MHz) instead of idling at µA. So the console UART is owned by
        // [`console::manager`], which builds it while USB is present and **drops** it on
        // unplug (releasing the refcount → STOP). `VBUS_SENSE` (PA12) is the gate: EXTI
        // line 12 wakes the MCU out of STOP on plug-in to bring the console back.
        // Pull-down so the sense line reads a defined low when unplugged (no false "USB
        // present" from a floating pin). When plugged, the FT231X drives PA12 high via its
        // CBUS3 output (a push-pull ~3.3 V logic level, not a 5 V divider), but only tens
        // of ms after power-up; the manager's VBUS poll covers that late assertion. PA12 is
        // also the USB DP pin, used here purely as a VBUS_SENSE GPIO (no USB peripheral).
        crate::console::install_logger(LOG_LEVEL);
        let vbus = ExtiInput::new(p.PA12, p.EXTI12, Pull::Down, Irqs);
        // DMA1_CH4 (TX) + DMA1_CH5 (RX) back the console UART, a different channel group from the
        // WS2812 strip's DMA1_CH3, so both coexist on DMA (overrun-proof console RX).
        spawner
            .spawn(crate::console::manager(p.USART1, p.PA9, p.PA10, p.DMA1_CH4, p.DMA1_CH5, vbus).unwrap());

        // I2C2 sensor bus: quiesce every device on it so the bus costs ~no idle current
        // (each may be absent on some boards, so NACKs are ignored). Order matters: the
        // ATSHA sleep is the LAST bus op, since any I2C traffic can re-wake it.
        let mut i2c_config = I2cConfig::default();
        i2c_config.frequency = Hertz::khz(100);
        i2c_bus_clear(p.PB10.reborrow(), p.PB11.reborrow());
        let i2c = I2c::new_blocking(p.I2C2, p.PB10, p.PB11, i2c_config);

        // TMP112 → shutdown (one-shot) so it doesn't free-run.
        let mut sensor = Tmp112::new(i2c, tmp112::ADDR_VPLUS);
        let _ = sensor.shutdown();
        let mut i2c = sensor.release(); // reclaim the bus for the rest of the hygiene

        // LIS2DH12 accelerometer → power-down (CTRL_REG1 = 0x00). Its POR default is
        // already power-down (~0.5 µA), but a prior configuration can persist across a
        // warm reset, so force it; the board wires only the accel's INT line, never
        // configures the part.
        let _ = i2c.blocking_write(ACCEL_ADDR, &[LIS2DH12_CTRL_REG1, 0x00]);

        // ATSHA204A crypto → Sleep (the last raw bus op, since any I2C traffic can re-wake
        // it). From here on the app's bus is [`AtshaGuard`]-wrapped, so every subsequent
        // transaction re-sleeps it automatically; no app has to remember.
        atsha_sleep(&mut i2c);

        // Re-wrap the (already shut-down) TMP112 on the ATSHA-guarded bus for the app.
        let sensor = Tmp112::new(AtshaGuard::new(i2c), tmp112::ADDR_VPLUS);

        Board {
            spawner,
            tmp112: sensor,
            led: p.PH1,
            // The Core Module button is active-high (press drives PA8 high; the
            // line is externally pulled down), so it uses a pull-down and
            // `Polarity::ActiveHigh` at the app.
            button: ExtiInput::new(p.PA8, p.EXTI8, Pull::Down, Irqs),
            strip_tim: p.TIM2,
            strip_data: p.PA1,
            strip_dma: p.DMA1_CH3,
            // LIS2DH12 INT1 (PB6): active-high push-pull, so pull-down + rising edge.
            accel_int: ExtiInput::new(p.PB6, p.EXTI6, Pull::Down, Irqs),
            kv: Nv::install(Storage::new(Flash::new_blocking(p.FLASH))),
            iwdg: p.IWDG,

            // SPIRIT1 radio resources. The driver (`Spirit1::new`) builds the
            // blocking SPI and drives SDN/CS itself; here we just hand over the
            // raw peripherals and the EXTI-bound nIRQ line.
            radio_sdn: p.PB7,
            radio_cs: p.PA15,
            radio_spi: p.SPI1,
            radio_sck: p.PB3,
            radio_mosi: p.PB5,
            radio_miso: p.PB4,
            radio_irq: ExtiInput::new(p.PA7, p.EXTI7, Pull::Up, Irqs),
        }
    }
}

/// Initialise clocks and low-power config for the Core Module, returning the
/// peripherals. (Used by [`Board::take`]; call directly only for custom setups.)
///
/// - **sysclk = HSI16 (16 MHz)**: fast enough for WS2812 PWM; only affects
///   run-mode current, STOP-mode idle is unchanged (clock off).
/// - **RTC ← LSE** (32.768 kHz crystal): the STOP-mode timekeeper / wake source.
/// - **`min_stop_pause = 0`**: any await length safely uses RTC-backed STOP.
/// - **debug clock gated in STOP** for real low-power current.
pub fn init() -> Peripherals {
    // Snapshot RCC_CSR (RTC-domain state + hardware reset-cause flags) BEFORE embassy's clock
    // init touches it, then clear the sticky reset flags so every boot reports its own cause.
    // Boot forensics: shows whether the LSE/RTC domain survived the reset and which hardware
    // reset fired. (Added for the 2026-07-05 bimodal-boot investigation, where it RULED OUT
    // the LSE/backup domain: every slow boot inherited a fully-matched domain + pin reset;
    // the proven cause is the EEPROM KV compaction CPU stall in init_session, ≈5.2 s, see
    // docs/storage.md. Kept: cheap, and reset-cause + domain state are generally useful.)
    // Read it back with [`preinit_csr`]; `examples/lse_probe.rs` decodes and logs it each boot.
    let csr = embassy_stm32::pac::RCC.csr().read();
    PREINIT_CSR.store(csr.0, core::sync::atomic::Ordering::Relaxed);
    embassy_stm32::pac::RCC.csr().modify(|w| w.set_rmvf(true));

    let mut config = embassy_stm32::Config::default();
    config.rcc.hsi = true;
    config.rcc.sys = Sysclk::HSI;
    config.rcc.ls = LsConfig::default_lse();
    // LSE crystal drive strength, selectable per build (TOWER_FEATURES=lse-drive-…). Default =
    // embassy's default_lse() = MediumHigh. Higher drive trades ~0.1-0.5 µA of STOP current
    // for oscillation robustness. (Swept Low/MediumHigh/High on the bench 2026-07-05: drive
    // has NO effect on the ~5.2 s bimodal-boot delay (that is the KV compaction stall), but
    // the knob is kept for crystal-robustness experiments on marginal boards.)
    #[cfg(any(
        feature = "lse-drive-low",
        feature = "lse-drive-medium-low",
        feature = "lse-drive-high"
    ))]
    if let Some(lse) = &mut config.rcc.ls.lse {
        #[cfg(feature = "lse-drive-low")]
        {
            lse.mode = embassy_stm32::rcc::LseMode::Oscillator(embassy_stm32::rcc::LseDrive::Low);
        }
        #[cfg(feature = "lse-drive-medium-low")]
        {
            lse.mode = embassy_stm32::rcc::LseMode::Oscillator(embassy_stm32::rcc::LseDrive::MediumLow);
        }
        #[cfg(feature = "lse-drive-high")]
        {
            lse.mode = embassy_stm32::rcc::LseMode::Oscillator(embassy_stm32::rcc::LseDrive::High);
        }
    }
    config.min_stop_pause = Duration::from_ticks(0);
    config.enable_debug_during_sleep = false;
    let p = embassy_stm32::init(config);

    apply_stop_tuning();

    p
}

/// Raw `RCC_CSR` as sampled by [`init`] before embassy's clock init ran: the RTC-domain
/// state the boot *inherited* (LSEON/LSERDY/LSEDRV/RTCSEL/RTCEN) plus this reset's cause
/// flags (PIN/POR/SFT/IWDG/…; cleared right after sampling, so they are per-boot). Decode
/// with `embassy_stm32::pac::rcc::regs::Csr(raw)`.
pub fn preinit_csr() -> u32 {
    PREINIT_CSR.load(core::sync::atomic::Ordering::Relaxed)
}

/// See [`preinit_csr`].
static PREINIT_CSR: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Base of the STM32L0's factory-programmed 96-bit unique device ID (RM0367 §28.2 "Unique
/// device ID registers (96 bits)"; `UID_BASE` = `0x1FF8_0050` in ST's CMSIS header for the
/// STM32L083xx, also embassy-stm32's metapac `UID` address for this chip).
const UID_BASE: usize = 0x1FF8_0050;
/// Offsets of the three ID words `U_ID[31:0]`, `U_ID[63:32]`, `U_ID[95:64]` from [`UID_BASE`]. **Not
/// contiguous on the L0**: the third word sits at `+0x14` (ST's `HAL_GetUIDw2`), with other
/// factory data in between. embassy-stm32's generic `uid::uid()` copies 12 contiguous bytes, so
/// on this family it returns the word at `+0x08` instead of `U_ID[95:64]`: don't use it here.
const UID_OFFSETS: [usize; 3] = [0x00, 0x04, 0x14];

/// The MCU's 96-bit factory unique device ID as its three 32-bit words, `U_ID[31:0]` first (read
/// from [`UID_OFFSETS`]). This is the device identity: stable for the life of the chip and
/// unique per STM32 (lot, wafer and position on the wafer). It is the STM32's ID, **not** the
/// on-board ATSHA204A's serial number, which the SDK deliberately does not use as an identity.
#[must_use]
pub fn unique_id_words() -> [u32; 3] {
    // SAFETY: word-aligned addresses in the always-readable factory system-memory area (the same
    // ones ST's HAL reads); read-only, no side effects.
    UID_OFFSETS.map(|off| unsafe { core::ptr::read_volatile((UID_BASE + off) as *const u32) })
}

/// The MCU's 96-bit factory unique device ID as 12 bytes: the three words of
/// [`unique_id_words`] in that order, each little-endian (the ID's byte order in memory).
#[must_use]
pub fn unique_id() -> [u8; 12] {
    let mut out = [0u8; 12];
    for (dst, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(unique_id_words()) {
        *dst = w.to_le_bytes();
    }
    out
}

/// Displays the unique ID as `XXXXXXXX-XXXXXXXX-XXXXXXXX`: the three words of
/// [`unique_id_words`] in hex, `U_ID[31:0]` first (the `uid:` line of `/system resource print` and
/// the push button's pairing log line).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct UniqueId(pub [u32; 3]);

impl UniqueId {
    /// This chip's ID.
    #[must_use]
    pub fn read() -> Self {
        Self(unique_id_words())
    }
}

impl core::fmt::Display for UniqueId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let [a, b, c] = self.0;
        write!(f, "{a:08X}-{b:08X}-{c:08X}")
    }
}

/// This unit's stable 32-bit radio address, derived from the STM32's 96-bit unique device ID
/// ([`unique_id`]): CRC-32/IEEE over the ID, clamped to never be 0 (`0` is the "gateway not yet
/// known" dest in the pairing JOIN_REQ, and the "unset / auto" sentinel of the
/// [`shell` `addr` setting](crate::shell::radio_addr)). Collisions across a fleet
/// are birthday-bounded (~2⁻³² per pair), acceptable for network addressing; an
/// operator can pin an explicit or random address instead (`system settings set addr`).
pub fn unique_id32() -> u32 {
    let addr = tower_protocol::crc::crc32_ieee(&unique_id());
    if addr == 0 { 1 } else { addr }
}

/// Which entropy source served [`rand_u32`]; see [`rand_source`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RandSource {
    /// The STM32L0 hardware TRNG (conditioned analog noise).
    Trng,
    /// The non-cryptographic software fallback ([`rand_u32_sw`]); the TRNG failed to come up.
    Fallback,
}

impl RandSource {
    /// Short label (`/system/resource print` `rng:` line, the `addr=random` reply).
    pub fn as_str(self) -> &'static str {
        match self {
            RandSource::Trng => "TRNG",
            RandSource::Fallback => "SW fallback",
        }
    }
}

/// Source of the most recent draw ([`rand_u32`], or [`trng_fill`]'s TRNG attempt): 0 = none
/// yet, 1 = TRNG, 2 = fallback / TRNG unavailable.
static RAND_SOURCE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Which source the most recent [`rand_u32`] used, probing once (one discarded draw) if nothing
/// has drawn yet. Lets a bench check prove the TRNG actually runs (`/system/resource print` →
/// `rng: TRNG`) instead of silently living on the fallback.
pub fn rand_source() -> RandSource {
    use core::sync::atomic::Ordering::Relaxed;
    if RAND_SOURCE.load(Relaxed) == 0 {
        let _ = rand_u32();
    }
    match RAND_SOURCE.load(Relaxed) {
        1 => RandSource::Trng,
        _ => RandSource::Fallback,
    }
}

/// A random `u32` from the STM32L0's hardware **TRNG** (analog-noise entropy source,
/// RM0367 §21), falling back to the non-cryptographic [`rand_u32_sw`] (with a one-time `warn!`
/// and [`rand_source`] reporting it) if the RNG can't be brought up, so the caller always gets a
/// usable non-zero value. No `.await` inside, so it runs to completion without another task
/// racing the RCC.
///
/// One-shot use only (minting a `system settings set addr=random`); the per-packet retransmit
/// backoff in the net layer stays on its own cheap software PRNG. Network addresses are not
/// secrets. Key material never comes from here: OTA pairing draws its X25519 ephemerals with
/// [`trng_fill`], which has no fallback.
pub fn rand_u32() -> u32 {
    use core::sync::atomic::Ordering::Relaxed;
    match rand_u32_trng() {
        Some(x) => {
            RAND_SOURCE.store(1, Relaxed);
            x
        }
        None => {
            if RAND_SOURCE.load(Relaxed) != 2 {
                log::warn!(target: "rng", "hardware TRNG unavailable - using the non-cryptographic fallback");
            }
            RAND_SOURCE.store(2, Relaxed);
            rand_u32_sw()
        }
    }
}

/// Fill `out` from the hardware **TRNG only** (key material: the OTA-pairing X25519
/// ephemerals). Never falls back to the software PRNG: `false` means the TRNG could not deliver
/// (HSI48 never started, a clock error, a stall, or a failed continuous test) and `out` must not
/// be used. Updates [`rand_source`]. Same bring-up / restore as [`rand_u32`] (see
/// `trng_words`), one bring-up for the whole buffer; no `.await` inside.
pub fn trng_fill(out: &mut [u8]) -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    let mut ok = true;
    for chunk in out.chunks_mut(16) {
        let mut w = [0u32; 4];
        let n = chunk.len().div_ceil(4);
        ok &= trng_words(&mut w[..n]);
        for (dst, word) in chunk.chunks_mut(4).zip(w) {
            dst.copy_from_slice(&word.to_le_bytes()[..dst.len()]);
        }
        w = [0; 4];
        core::hint::black_box(&w);
        if !ok {
            break;
        }
    }
    if !ok {
        out.fill(0);
    }
    RAND_SOURCE.store(if ok { 1 } else { 2 }, Relaxed);
    ok
}

/// One conditioned word from the hardware RNG (non-zero, for an address), or `None` if it can't
/// be brought up.
fn rand_u32_trng() -> Option<u32> {
    let mut w = [0u32];
    (trng_words(&mut w) && w[0] != 0).then_some(w[0])
}

/// Fill `out` with conditioned words from the hardware RNG; `false` if it can't be brought up
/// or a word fails the continuous test.
///
/// The L0's RNG is clocked from **HSI48**, and HSI48 only starts with the VREFINT reference routed
/// to it (`SYSCFG_CFGR3.ENREF_HSI48`, which needs the SYSCFG clock). embassy's L0 RCC init already
/// does exactly that (`init_hsi48` sets `ENREF_HSI48` + `EN_VREFINT` and starts HSI48) because
/// its default `rcc.hsi48` is `Some` (this board keeps the default), and it re-runs on every STOP
/// exit. This function mirrors the same sequence anyway (so it does not depend on that config),
/// and snapshots + **restores** everything it touches (the SYSCFG clock, the `CFGR3` reference
/// bits, HSI48 on/off, the `CLK48SEL` mux, the RNG clock) instead of forcing HSI48 off as it once
/// did (which fought embassy's clock state until the next STOP exit). STOP power is unaffected
/// either way: HSI48 stops in STOP, and with `PWR_CR.ULP` set VREFINT is off there too.
///
/// **Continuous test** (the FIPS 140-2 style "stuck output" check): a word equal to the one before
/// it fails the whole draw. A healthy source repeats a 32-bit word with probability 2⁻³², and a
/// stuck one must never feed a key.
fn trng_words(out: &mut [u32]) -> bool {
    use embassy_stm32::pac::rcc::vals::Clk48sel;
    use embassy_stm32::pac::{RCC, RNG, SYSCFG};

    let syscfg_was_on = RCC.apb2enr().read().syscfgen();
    RCC.apb2enr().modify(|w| w.set_syscfgen(true));
    let cfgr3_was = SYSCFG.cfgr3().read();
    SYSCFG.cfgr3().modify(|w| {
        w.set_enref_hsi48(true);
        w.set_en_vrefint(true);
    });
    let hsi48_was_on = RCC.crrcr().read().hsi48on();
    let clk48_was = RCC.ccipr().read().clk48sel();
    RCC.crrcr().modify(|w| w.set_hsi48on(true));

    let mut ok = false;
    // Bounded wait for HSI48 (it also covers the VREFINT start-up when the reference was off).
    let mut spins = 0u32;
    let mut ready = true;
    while !RCC.crrcr().read().hsi48rdy() {
        spins += 1;
        if spins > 100_000 {
            ready = false;
            break;
        }
    }
    if ready {
        RCC.ccipr().modify(|w| w.set_clk48sel(Clk48sel::HSI48));
        RCC.ahbenr().modify(|w| w.set_rngen(true));
        RNG.cr().write(|w| w.set_rngen(true)); // rngen=1, ie=0, ced=0 (clock-check on)

        // Wait for each conditioned word. A seed error (SEIS) means "clear it, toss a word,
        // retry"; a clock error (CEIS), a stall or a repeated word gives up (→ `false`).
        let mut prev: Option<u32> = None;
        let mut filled = 0;
        'read: for _ in 0..64 + out.len() {
            spins = 0;
            loop {
                let sr = RNG.sr().read();
                if sr.ceis() {
                    break 'read;
                }
                if sr.seis() {
                    RNG.sr().modify(|w| w.set_seis(false));
                    let _ = RNG.dr().read();
                    prev = None; // the continuous test restarts after a reseed
                    break; // retry the outer loop
                }
                if sr.drdy() {
                    let w = RNG.dr().read();
                    if prev == Some(w) {
                        break 'read; // stuck output: fail the whole draw
                    }
                    prev = Some(w);
                    out[filled] = w;
                    filled += 1;
                    if filled == out.len() {
                        ok = true;
                        break 'read;
                    }
                    break; // next word
                }
                spins += 1;
                if spins > 100_000 {
                    break 'read;
                }
            }
        }
        RNG.cr().write(|w| w.set_rngen(false));
        RCC.ahbenr().modify(|w| w.set_rngen(false));
    }

    // Hand the clock tree back exactly as found (embassy's view of it stays true).
    RCC.ccipr().modify(|w| w.set_clk48sel(clk48_was));
    if !hsi48_was_on {
        RCC.crrcr().modify(|w| w.set_hsi48on(false));
    }
    SYSCFG.cfgr3().modify(|w| {
        w.set_enref_hsi48(cfgr3_was.enref_hsi48());
        w.set_en_vrefint(cfgr3_was.en_vrefint());
    });
    if !syscfg_was_on {
        RCC.apb2enr().modify(|w| w.set_syscfgen(false));
    }
    if !ok {
        out.fill(0);
    }
    ok
}

/// Software fallback for [`rand_u32`], **not cryptographic**. Mixes the chip UID, the
/// uptime tick, and a rolling internal state, then runs one xorshift step (host-tested in
/// [`tower_shell::xorshift32_nonzero`], which forces the seed non-zero). Used only if
/// the hardware TRNG is somehow unavailable (reported by [`rand_source`]); successive calls
/// (and different units) diverge.
fn rand_u32_sw() -> u32 {
    use core::sync::atomic::{AtomicU32, Ordering};
    static STATE: AtomicU32 = AtomicU32::new(0);
    let seed =
        STATE.load(Ordering::Relaxed) ^ unique_id32() ^ (embassy_time::Instant::now().as_ticks() as u32);
    let x = tower_shell::xorshift32_nonzero(seed);
    STATE.store(x, Ordering::Relaxed);
    x
}

/// Assert the STM32L0 STOP-mode power tuning in `PWR_CR`:
/// - **LPSDSR**: put the voltage regulator in *low-power* mode during deep sleep
///   (embassy's L0 `enter_stop` sets only PDDS/CWUF, otherwise leaving the *main*
///   regulator powered in STOP: tens to hundreds of µA of avoidable draw);
/// - **ULP**: disable VREFINT in Stop (its buffer costs ~1.5 µA).
///
/// **Must be re-applied after every wake.** embassy's `exit_stop` re-inits RCC on each
/// wake (`rcc::reinit_saved` → `rcc/l.rs` voltage-scale step), which does a *full-register*
/// `PWR.cr().write(set_vos)`; that zeroes LPSDSR (bit 0) and ULP (bit 9). So a single
/// init-time write only holds for the first STOP; from the second STOP on the bits are
/// gone. [`crate::console::manager`] re-calls this on each wake of its VBUS-poll loop, so
/// the idle (unplugged) STOP always re-enters with the bits set. Caveat: with `ULP` set,
/// VREFINT is unavailable for a short window after wake (regulator/VREFINT startup), which is fine
/// here as the SDK uses no ADC/VREFINT; code that adds battery sensing must account for it.
pub(crate) fn apply_stop_tuning() {
    embassy_stm32::pac::PWR.cr().modify(|w| {
        w.set_lpsdsr(embassy_stm32::pac::pwr::vals::Mode::LOW_POWER_MODE);
        w.set_ulp(true);
    });
}

/// Free the I²C2 bus if a slave holds SDA low at boot: a device that was mid-byte when a warm
/// reset hit the MCU still drives a 0, waiting for clocks that will never come, and every later
/// transaction then fails until a power cycle (an accelerometer interrupt latched behind such a
/// bus can never be cleared). The I²C-bus specification's "bus clear" (UM10204): clock SCL up
/// to nine times until the slave lets SDA go, then send a STOP. A free bus (SDA high, the normal
/// case) is not touched. The pins are released (disconnected) when this returns, for the I²C
/// peripheral to take over.
fn i2c_bus_clear(scl: Peri<'_, PB10>, sda: Peri<'_, PB11>) {
    // Half an SCL period at the bus's 100 kHz.
    const HALF: Duration = Duration::from_micros(5);
    let mut sda = Flex::new(sda);
    sda.set_high();
    sda.set_as_input_output_pull(Speed::Low, Pull::Up); // released: reads the bus
    embassy_time::block_for(HALF);
    if sda.is_high() {
        return;
    }
    let mut scl = Flex::new(scl);
    scl.set_high();
    scl.set_as_input_output_pull(Speed::Low, Pull::Up);
    for _ in 0..9 {
        if sda.is_high() {
            break;
        }
        scl.set_low();
        embassy_time::block_for(HALF);
        scl.set_high();
        embassy_time::block_for(HALF);
    }
    // STOP: SDA rises while SCL is high.
    scl.set_low();
    embassy_time::block_for(HALF);
    sda.set_low();
    embassy_time::block_for(HALF);
    scl.set_high();
    embassy_time::block_for(HALF);
    sda.set_high();
    embassy_time::block_for(HALF);
}

/// Put the on-board **ATSHA204A** crypto (I²C 0x64) back to Sleep (word address `0x01`,
/// idle ~30 nA). Awake it draws far more (up to ~200 µA idle), enough to dwarf the µA
/// STOP-mode floor. **Assume any I²C transaction on the shared bus can wake it**, so it
/// must be re-slept after *every* other transaction; the [`AtshaGuard`] bus wrapper does
/// this automatically, so most code never calls this directly. NACK handling (incl. the
/// just-woken case) is [`atsha_sleep_retry`]'s.
///
/// Also **learns whether the chip is fitted**: [`Board::take`] calls this right after bus
/// traffic that is guaranteed to wake a fitted ATSHA (the TMP112 pointer write holds SDA low
/// ~80 µs), so a Sleep that is NACKed even after the wake delay means "no ATSHA on this board
/// variant", and the [`AtshaGuard`] then skips the delayed retry (and its ~3 ms busy-wait).
pub fn atsha_sleep(i2c: &mut I2c<'static, Blocking, Master>) {
    let acked = atsha_sleep_retry(i2c, true);
    ATSHA_PRESENT.store(acked, core::sync::atomic::Ordering::Relaxed);
}

/// Whether an ATSHA204A answered the last explicit [`atsha_sleep`] (boot); `true` until then,
/// so an unknown board errs on the side of the retry.
static ATSHA_PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// ATSHA204A wake-to-command delay (datasheet t_WHI = 2.5 ms) plus margin.
const ATSHA_WAKE_DELAY: Duration = Duration::from_millis(3);

/// Send the ATSHA204A Sleep command, retrying **once after ~3 ms** if it is NACKed.
///
/// Why the retry: the ATSHA wakes on SDA held low for ≥ 60 µs (t_WLO), and ordinary traffic
/// does that. At 100 kHz a `0x00` byte plus its ACK holds SDA low ~90 µs (the TMP112's pointer
/// `0x00` before every temperature read, the boot LIS2DH12 `CTRL_REG1 = 0x00` write). A chip just
/// woken that way ignores the bus for t_WHI = 2.5 ms, so a Sleep sent ~100 µs later is NACKed
/// (indistinguishable from "already asleep"), and the chip idles awake until its own ~1.3 s
/// watchdog (I_IDLE is hundreds of µA: at 1 read/min that is ~4 µA average, the whole 5.1 µA STOP
/// floor again). Retrying after t_WHI lands the Sleep on a listening chip.
///
/// Cost: a ~3 ms busy-wait after every NACKed attempt, i.e. after most guarded transactions, since
/// an asleep chip NACKs too (a NACKed address phase never wakes it: the address byte `0xC8` has
/// no long zero run and no data byte follows). Boards without the chip skip it (`retry = false`,
/// see [`atsha_sleep`]). Returns whether either attempt was ACKed (= a chip is fitted).
/// **Needs a PPK2 trace to confirm** the wake + recovered µA on hardware (bench TODO).
fn atsha_sleep_retry<B: I2cTrait>(bus: &mut B, retry: bool) -> bool {
    use embedded_hal::i2c::{Error as _, ErrorKind};
    match bus.write(ATSHA_ADDR, &[0x01]) {
        Ok(()) => true,
        Err(e) if retry && matches!(e.kind(), ErrorKind::NoAcknowledge(_)) => {
            embassy_time::block_for(ATSHA_WAKE_DELAY);
            bus.write(ATSHA_ADDR, &[0x01]).is_ok()
        }
        Err(_) => false,
    }
}

/// I²C bus wrapper that re-sleeps the on-board **ATSHA204A** (0x64) after **every**
/// transaction to another device. The ATSHA shares the I²C2 bus and, conservatively,
/// *any* traffic can wake it into ~200 µA idle (its watchdog only auto-sleeps it ~1.3 s
/// after a wake), which would dwarf the µA STOP-mode floor. Wrapping the bus makes the
/// "re-sleep after each transaction" policy automatic, so the sensor drivers and apps
/// can't forget it. Transactions addressed to the ATSHA itself are passed through
/// untouched, so a deliberate crypto sequence still manages its own wake/sleep. The
/// re-sleep retries once after the chip's wake delay ([`atsha_sleep_retry`]), adding a
/// ~3 ms busy-wait to each guarded transaction on boards where the chip is fitted.
///
/// It implements [`embedded_hal::i2c::I2c`], so it drops in wherever a raw bus went (the
/// [`Tmp112`] / [`Lis2dh12`](crate::lis2dh12::Lis2dh12) drivers are
/// generic over it). Use [`into_inner`](Self::into_inner) for deliberate *unguarded*
/// access (e.g. a raw bus scan).
pub struct AtshaGuard<T> {
    inner: T,
}

impl<T> AtshaGuard<T> {
    /// Wrap `inner` so every non-ATSHA transaction is followed by an ATSHA sleep.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Unwrap and return the raw (unguarded) bus.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: ErrorType> ErrorType for AtshaGuard<T> {
    type Error = T::Error;
}

impl<T: I2cTrait> I2cTrait for AtshaGuard<T> {
    fn transaction(&mut self, address: u8, operations: &mut [Operation<'_>]) -> Result<(), Self::Error> {
        let r = self.inner.transaction(address, operations);
        // Re-sleep the ATSHA after traffic to any *other* device. Issued on `inner`
        // directly (never recurses through the guard); a NACK gets one delayed retry (the chip
        // may have just been woken by this very transaction), then is ignored.
        if address != ATSHA_ADDR {
            let fitted = ATSHA_PRESENT.load(core::sync::atomic::Ordering::Relaxed);
            let _ = atsha_sleep_retry(&mut self.inner, fitted);
        }
        r
    }
}
