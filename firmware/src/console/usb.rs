//! The console of the Core Module **rev 1.3** (feature `core-module-r1`): the framed link over a
//! USB CDC-ACM serial port on the STM32's own USB peripheral, brought up only while a USB host is
//! there, so the board still idles in STOP (µA) on a battery.
//!
//! The first Core Module (BigClown rev 1.x, schematic "CORE MODULE rev 1.3", 2016) has no
//! USB-UART bridge: the micro-USB connector's D-/D+ go to PA11/PA12 through 33 Ω (R8/R9), with a
//! TPD2E001 ESD array (U5) and 47 pF (C18/C20) at the connector. The framing, the queue, the
//! writer and the RX router are the ones of the r2 board ([`super`]); only the link differs, and
//! the way the console finds out that a host is there.
//!
//! # Finding the host without a VBUS line
//!
//! The r2 board gates its console on `VBUS_SENSE` (PA12, driven by the bridge). Rev 1.3 has no
//! VBUS line to the MCU at all: +5 V only feeds the LDO, the power-path switch (Q2 drives the
//! dual P-FET Q1 that hands VDD to the LDO) and the header's `BAT_OFF` / `VDD_OFF` outputs. And
//! the obvious substitute, "enable the USB device and see whether a host enumerates it", is
//! **unsafe** there: the D+ pull-up (1.5 kΩ, `USB_BCDR.DPPU`) with no cable feeds the VBUS net
//! through U5 (its VCC pin sits on VBUS, and its steering diodes conduct from an I/O line to VCC
//! above VCC + VF), which lifts VBUS far enough to switch Q2 and Q1 on and can assert `BAT_OFF`:
//! a Battery Module then disconnects its cells and the board browns out, boots, enables the
//! pull-up again, and loops. A self-powered USB device must not drive its pull-up without VBUS
//! (USB 2.0, 7.1.5), and that is the rule this module keeps: **the pull-up is only enabled after
//! the probe below has seen VBUS.**
//!
//! **The pull-up probe** ([`host_probe`]) asks the line instead, in a way that stays safe: every
//! [`POLL`] it powers the USB transceiver up (every interrupt masked), switches its own D+ pull-up
//! on for ~1.5 µs, reads the D+ line state back from `USB_FNR.RXDP` and switches everything off:
//! * **a host:** VBUS is up, so U5's diode is reverse-biased, and the 1.5 kΩ pull-up against the
//!   host's 15 kΩ pull-down puts D+ at ~3.0 V: it reads **high** (on a rev 1.3 board on a Mac,
//!   `FNR` read `0x8000`, D+ high and D- low, every time);
//! * **no VBUS:** U5's diode from D+ to its VCC pin (VBUS, at 0 V) clamps D+ to a diode drop,
//!   under 1 V even with the pull-up's ~2 mA through it: it reads **low**;
//! * **a charger** (VBUS, no host) reads high: harmless, VBUS is there, and the session below ends
//!   when nobody configures the device.
//!
//! The pull-up is on for less than the 2.5 µs a hub needs to register a connect (USB 2.0,
//! 7.1.7.3, TDCNN), so a host never sees a probe as an attach, and with no VBUS the clamp takes
//! ~3 nC from it, nothing the VBUS net (≥ 210 nF) notices. Two "present" readings [`DEBOUNCE`]
//! apart start a session. A probe costs a wake from STOP and a few µs of CPU. The "no VBUS" side
//! relies on the clamp; check it on a battery-powered board (the probe must read low and the
//! board must stay in STOP).
//!
//! # A session
//!
//! [`session`] enables HSI48's clock recovery from the USB SOF (CRS), puts the regulator in main
//! mode for Sleep (the STOP tuning's `LPSDSR` would leave HSI48 and the transceiver on the
//! low-power regulator during the session's WFI), builds the USB device (which enables the
//! pull-up: the host now sees an attach) and runs it:
//! * not configured within [`NO_HOST_TIMEOUT`] (a charger, a sleeping host, a probe that got it
//!   wrong): the session ends and the next one waits a back-off ([`BACKOFF_MIN`], doubling to
//!   [`BACKOFF_MAX`]; reset by an unplug or a configured session), so a port that never
//!   enumerates is not re-attached every poll. Only a configuration proves a host: the
//!   peripheral reports a bus reset of its own when it is enabled (the line is SE0 until the
//!   pull-up has raised D+), and a suspend 3 ms later while the host is still debouncing the
//!   attach;
//! * after the configuration, a bus **suspend** (3 ms without SOF: the cable pulled, or the host
//!   asleep) ends the session at once, which also cuts the window in which the pull-up could feed
//!   a falling VBUS;
//! * while the device is configured **and a host program holds the port open** (DTR or RTS
//!   asserted: opening a port asserts both on Linux and macOS, and the `tower` CLI drives both
//!   as its run baseline) the console is up ([`super::CONSOLE_UP`]), [`LINE_OPEN_SETTLE`] after
//!   the port opened: [`super::writer_loop`] and [`super::rx_loop`] run over the CDC endpoints,
//!   and each opening of the port starts with a fresh `Hello`. While the port is closed, the
//!   queue keeps its oldest lines (the boot backlog reaches the host when it opens the port) and
//!   producers drop the newest.
//!
//! The USB peripheral holds embassy's STOP refcount for the whole session (its clock is enabled
//! with `Stop1`), so the executor uses WFI while USB runs; the board is on USB power then (the
//! LDO supplies VDD and `BAT_OFF` disconnects a Battery Module). Teardown powers the peripheral
//! down and releases that refcount by hand (embassy's driver has no `Drop`), so an unplugged
//! board is back in STOP.
//!
//! A host that opened the port but stopped reading can't stall the app: every packet write is
//! bounded by [`WRITE_TIMEOUT`]. On a timeout the link counts as stalled: the console goes down
//! for the producers (drop-newest, the queue is emptied and counted as dropped) while the writer
//! waits for the host to take the pending packet, then resyncs the host's decoder with a lone
//! `0x00` and carries on.
//!
//! # Reset emulation
//!
//! The r2 board's bridge drives NRST and BOOT0 from RTS/DTR, and `tower` uses that to reset the
//! board or enter the ROM bootloader (`crates/flasher/src/port.rs`). Rev 1.3 has no such wiring,
//! so [`lines_loop`] decodes the same line states from the CDC `SET_CONTROL_LINE_STATE`
//! requests: (RTS, DTR) = (on, off) is "RESET asserted", (off, on) is "BOOT0 high". When the
//! lines leave the RESET state after holding it at least [`RESET_MIN_HOLD`], the console waits
//! [`LINE_SETTLE`] (the host writes the two lines one request at a time, so a transient
//! (on, on) sits between (on, off) and (off, on)) and reboots: into the ROM bootloader (its USB
//! DFU device, which `tower flash` then programs) if BOOT0 is high, into the application
//! otherwise. Opening or closing the port ((off, off) and (on, on)) never resets, and neither
//! does a shorter (on, off) transient of a host that sets the two lines in separate requests.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, Ordering};

use cortex_m::peripheral::SCB;
use embassy_futures::select::{Either3, select3};
use embassy_stm32::interrupt::InterruptExt;
use embassy_stm32::pac::crs::vals::Syncsrc;
use embassy_stm32::pac::gpio::vals::{Moder, Pupdr};
use embassy_stm32::pac::pwr::vals::Mode;
use embassy_stm32::pac::{CRS, GPIOA, PWR, RCC};
use embassy_stm32::peripherals::{PA11, PA12, USB};
use embassy_stm32::usb::{Driver, InterruptHandler};
use embassy_stm32::{Peri, bind_interrupts, interrupt};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_deadline, with_timeout};
use embassy_usb::class::cdc_acm::{CdcAcmClass, ControlChanged, Receiver, Sender, State};
use embassy_usb::{Builder, Handler};

use super::{LinkRx, LinkTx, drain_queue, rx_loop, set_console_up, writer_loop};

bind_interrupts!(struct UsbIrqs {
    USB => InterruptHandler<USB>;
});

/// USB vendor id of the console: STMicroelectronics, with the product id of its virtual COM port,
/// the pair the legacy TOWER SDK's rev 1.3 CDC console used (twr-sdk `usbd_desc.c`). The `tower`
/// CLI recognizes the port by it plus the serial number.
const VID: u16 = 0x0483;
/// USB product id of the console (ST's virtual COM port), see [`VID`].
const PID: u16 = 0x5740;
const MANUFACTURER: &str = "HARDWARIO";
const PRODUCT: &str = "TOWER Core Module";
/// Serial number prefix; the 96-bit unique device id follows as 24 hex digits (the words of
/// [`unique_id_words`](crate::board::unique_id_words) in order). `core-module` is what the `tower`
/// CLI keys a Core Module on, as in the r2 bridge's `hio-core-module-<id>`.
const SERIAL_PREFIX: &str = "hio-core-module-r1-";
const SERIAL_LEN: usize = SERIAL_PREFIX.len() + 24;
/// Bulk packet size of the CDC data endpoints (the full-speed maximum).
const MAX_PACKET: u16 = 64;

/// Idle poll period of the probe: the delay between plugging a host in and the console coming
/// up, against a wake from STOP per poll.
const POLL: Duration = Duration::from_secs(3);
/// Gap between the two "present" probes that start a session.
const DEBOUNCE: Duration = Duration::from_millis(50);
/// A host configures a device it sees attach within a few hundred ms (a 100 ms attach debounce,
/// the reset, then enumeration); not configured within this means no host is enumerating us (a
/// charger, a sleeping host). Only a configuration counts as a host: the peripheral reports a bus
/// reset of its own at enable time (the line is SE0 until the pull-up has raised D+; seen on a
/// rev 1.3 board one millisecond after `build`, then a suspend 3 ms later), so a reset proves
/// nothing. This is also the bound on a session a wrong probe started with no VBUS, where D+
/// stays clamped low through the ESD array and nothing ever enumerates.
const NO_HOST_TIMEOUT: Duration = Duration::from_secs(3);
/// First wait before re-attaching to a port that did not enumerate us; doubles per failure.
const BACKOFF_MIN: Duration = Duration::from_secs(2);
/// Longest wait between attach attempts on a port that keeps not enumerating us.
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Bound on one packet write; see the module docs (a host that stopped reading).
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);
/// Shortest (RTS, DTR) = (on, off) that counts as a reset pulse. `tower` holds it 50 ms (into the
/// bootloader) or 100 ms (into the application); a host setting the two lines in separate
/// requests passes through it for a moment only.
const RESET_MIN_HOLD: Duration = Duration::from_millis(20);
/// Settle after the lines leave the RESET state, before BOOT0 is sampled; see the module docs.
const LINE_SETTLE: Duration = Duration::from_millis(30);
/// Transceiver start-up before the probe's pull-up, in CPU cycles: t_STARTUP is 1 µs (RM0367,
/// `USB_CNTR.PDWN`); 2 µs at the 16 MHz HSI16 system clock.
const PROBE_STARTUP_CYCLES: u32 = 32;
/// How long the probe's pull-up is on before D+ is read, in CPU cycles: ~1.5 µs at 16 MHz, six
/// time constants of 1.5 kΩ against ~150 pF of line, and under the 2.5 µs a hub needs to see a
/// connect.
const PROBE_PULLUP_CYCLES: u32 = 24;
/// After a host program opens the port, wait this long before the console's opening frames: an
/// opening program may still be flushing its input (the `tower` CLI clears it 50 ms after setting
/// its lines, `RUN_SETTLE`), which would discard the `Hello`.
const LINE_OPEN_SETTLE: Duration = Duration::from_millis(100);
/// PA12, the D+ line.
const DP: usize = 12;
/// PA11, the D- line.
const DM: usize = 11;

// Builder buffers and the CDC state, re-borrowed by each [`session`]: sound because at most one
// session (the only user) is alive at a time. Sizes: the CDC-ACM configuration descriptor with
// its interface association is 75 B, the BOS (USB 2.1) a 5-byte header. The control buffer also
// carries the string descriptors embassy-usb encodes on request, and the longest is the serial
// number: 2 + 2 × 43 = 88 B (embassy-usb asserts it fits, so a short buffer is a panic at
// enumeration, not a truncated string).
static mut CONFIG_DESC: [u8; 96] = [0; 96];
static mut BOS_DESC: [u8; 16] = [0; 16];
static mut CONTROL_BUF: [u8; 96] = [0; 96];
const _: () = assert!(2 + 2 * SERIAL_LEN < 96);
static mut CDC_STATE: State<'static> = State::new();
static mut BUS_WATCH: BusWatch = BusWatch;
static mut SERIAL: [u8; SERIAL_LEN] = [0; SERIAL_LEN];

/// The device is configured (the CDC endpoints are live).
static CONFIGURED: AtomicBool = AtomicBool::new(false);
/// The device was configured at some point in this session (it reached a host).
static CONFIGURED_SEEN: AtomicBool = AtomicBool::new(false);
/// The bus is suspended.
static SUSPENDED: AtomicBool = AtomicBool::new(false);
/// A packet write timed out and the host has not taken it yet (see [`CdcTx::packet`]).
static STALLED: AtomicBool = AtomicBool::new(false);
/// Something [`supervisor`] decides on changed: a bus state ([`BusWatch`]) or the port's lines.
static BUS_EVENT: Signal<CriticalSectionRawMutex, ()> = Signal::new();

type UsbDriver = Driver<'static, USB>;

/// How a [`session`] ended.
enum End {
    /// No host configured the device in [`NO_HOST_TIMEOUT`].
    NoHost,
    /// The bus suspended after a host had configured the device (unplugged, or the host is
    /// asleep).
    Suspended,
}

/// USB-host-gated **dynamic** console of the rev 1.3 board. Owns the USB peripheral and its
/// pins for the whole run: probes for a host while idle ([`host_probe`]), runs a USB session while a
/// host is there ([`session`]), and between sessions keeps the peripheral powered down so the
/// executor reaches STOP. Spawned by [`board::Board::take`](crate::board::Board::take).
///
/// `usb`/`dm`/`dp` are held to reserve the peripherals; each session re-acquires them with
/// `steal()` (sound: exactly one session is alive at a time).
#[embassy_executor::task]
pub async fn manager(usb: Peri<'static, USB>, dm: Peri<'static, PA11>, dp: Peri<'static, PA12>) {
    let _reserved = (usb, dm, dp);
    quiesce();
    let mut backoff = BACKOFF_MIN;
    let mut retry_at: Option<Instant> = None;
    loop {
        // Each wake re-asserts the STOP power tuning (embassy's wake path clears it; see
        // `board::apply_stop_tuning`), exactly as the r2 manager's VBUS poll does.
        crate::board::apply_stop_tuning();
        if !host_probe() {
            // No host: forget the back-off (the next plug is a new port), and drain what was
            // queued while there was no writer, like the r2 manager's unplugged poll.
            backoff = BACKOFF_MIN;
            retry_at = None;
            drain_queue();
            Timer::after(POLL).await;
            continue;
        }
        if retry_at.is_some_and(|t| Instant::now() < t) {
            Timer::after(POLL).await;
            continue;
        }
        Timer::after(DEBOUNCE).await;
        if !host_probe() {
            continue;
        }
        let end = session().await;
        if matches!(end, End::Suspended) && CONFIGURED_SEEN.load(Ordering::Relaxed) {
            // A host had us: whatever comes next (an unplug, the host waking up) starts afresh.
            backoff = BACKOFF_MIN;
            retry_at = None;
        } else {
            // Nobody enumerated us (a charger, a sleeping host): wait before attaching again.
            log::debug!(target: "usb", "no host enumerated the console, next try in {} s", backoff.as_secs());
            retry_at = Some(Instant::now() + backoff);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
        // Let VBUS settle before the next probe: right after an unplug the VBUS net is still
        // discharging, and a clamp at a few volts would read as "present".
        Timer::after(POLL).await;
    }
}

/// One USB session: build and run the CDC-ACM device until [`supervisor`] ends it, then power the
/// peripheral down again. Only called after [`host_probe`] saw VBUS (the pull-up goes on here).
async fn session() -> End {
    CONFIGURED.store(false, Ordering::Relaxed);
    CONFIGURED_SEEN.store(false, Ordering::Relaxed);
    SUSPENDED.store(false, Ordering::Relaxed);
    STALLED.store(false, Ordering::Relaxed);
    BUS_EVENT.reset();
    // The session runs on WFI (the USB clock holds the STOP refcount), and `apply_stop_tuning`'s
    // LPSDSR puts the regulator in low-power mode in Sleep as well as in Stop (RM0367, PWR_CR):
    // not meant for HSI48 and the USB transceiver. Main regulator while USB runs; the manager
    // re-applies the tuning at its next poll, after the teardown.
    PWR.cr().modify(|w| w.set_lpsdsr(Mode::MAIN_MODE));
    crs_start();

    // SAFETY: one session at a time (the manager awaits this future), so the peripherals stolen
    // here and the statics re-borrowed below have no other user; the previous session's borrows
    // all ended with its scope.
    let driver = Driver::new(
        unsafe { USB::steal() },
        UsbIrqs,
        unsafe { PA12::steal() },
        unsafe { PA11::steal() },
    );
    let mut config = embassy_usb::Config::new(VID, PID);
    config.manufacturer = Some(MANUFACTURER);
    config.product = Some(PRODUCT);
    config.serial_number = Some(serial_number());
    // On USB the board runs from the bus: the LDO supplies VDD and `BAT_OFF` disconnects a
    // Battery Module. 100 mA covers the MCU and a transmitting radio.
    config.max_power = 100;
    let (config_desc, bos_desc, control_buf, watch, state) = unsafe {
        addr_of_mut!(CDC_STATE).write(State::new());
        (
            &mut *addr_of_mut!(CONFIG_DESC),
            &mut *addr_of_mut!(BOS_DESC),
            &mut *addr_of_mut!(CONTROL_BUF),
            &mut *addr_of_mut!(BUS_WATCH),
            &mut *addr_of_mut!(CDC_STATE),
        )
    };
    // The builder lives in its own block: a local that is merely moved out of keeps its slot in
    // this future's state until its scope ends, and the builder is ~200 B of the console
    // manager's static task.
    let (mut device, class) = {
        let mut builder = Builder::new(driver, config, config_desc, bos_desc, &mut [], control_buf);
        builder.handler(watch);
        let class = CdcAcmClass::new(&mut builder, state, MAX_PACKET);
        // `build` starts the peripheral and enables the D+ pull-up: from here the host sees us.
        (builder.build(), class)
    };
    let (sender, receiver, control) = class.split_with_control();
    let mut tx = CdcTx {
        sender,
        resync: false,
    };
    let mut rx = CdcRx { receiver };

    let end = match select3(
        device.run(),
        lines_loop(&control),
        supervisor(&mut tx, &mut rx, &control),
    )
    .await
    {
        Either3::First(never) | Either3::Second(never) => match never {},
        Either3::Third(end) => end,
    };
    set_console_up(false);
    drop((tx, rx, control, device));
    power_down();
    crs_stop();
    end
}

/// Decide the session's course from the bus state and the port's lines: wait for a host, run the
/// console while a host program holds the port open, end the session on a suspend (see the
/// module docs).
async fn supervisor(tx: &mut CdcTx, rx: &mut CdcRx, control: &ControlChanged<'static>) -> End {
    if with_timeout(NO_HOST_TIMEOUT, wait_for(|| CONFIGURED.load(Ordering::Relaxed)))
        .await
        .is_err()
    {
        return End::NoHost;
    }
    loop {
        wait_for(|| bus_suspended() || console_wanted(control)).await;
        if bus_suspended() {
            return End::Suspended;
        }
        Timer::after(LINE_OPEN_SETTLE).await;
        if !console_wanted(control) {
            continue;
        }
        STALLED.store(false, Ordering::Relaxed);
        tx.resync = false;
        // The writer leads with a lone 0x00 and a fresh `Hello`, so the host program that just
        // opened the port resyncs its decoder and `seq` tracking.
        set_console_up(true);
        let _ = select3(
            writer_loop(tx),
            rx_loop(rx),
            wait_for(|| bus_suspended() || !console_wanted(control)),
        )
        .await;
        set_console_up(false);
    }
}

/// The bus suspended after a host had configured us (an earlier suspend is the idle bus of the
/// host's attach debounce, not a host going away).
fn bus_suspended() -> bool {
    CONFIGURED_SEEN.load(Ordering::Relaxed) && SUSPENDED.load(Ordering::Relaxed)
}

/// The device is configured and a host program holds the port open (DTR or RTS asserted).
fn console_wanted(control: &ControlChanged<'static>) -> bool {
    CONFIGURED.load(Ordering::Relaxed) && (control.dtr() || control.rts())
}

/// Wait until `cond` holds, re-checking it on every [`BUS_EVENT`]. Single waiter: only
/// [`supervisor`] waits on the signal, one `wait_for` at a time.
async fn wait_for(cond: impl Fn() -> bool) {
    while !cond() {
        BUS_EVENT.wait().await;
    }
}

/// Follow the port's RTS/DTR: wake [`supervisor`] on every change and emulate the r2 board's
/// reset circuit (see the module docs, "Reset emulation"). Line states only count while the
/// device is configured; a bus reset clears them.
async fn lines_loop(control: &ControlChanged<'static>) -> ! {
    let mut reset_since: Option<Instant> = None;
    loop {
        control.control_changed().await;
        BUS_EVENT.signal(());
        if !CONFIGURED.load(Ordering::Relaxed) {
            reset_since = None;
            continue;
        }
        let (rts, dtr) = (control.rts(), control.dtr());
        if rts && !dtr {
            // RESET asserted: remember since when, act on the release.
            reset_since.get_or_insert_with(Instant::now);
            continue;
        }
        let Some(since) = reset_since.take() else {
            continue;
        };
        if since.elapsed() < RESET_MIN_HOLD {
            continue; // a transient between two separate line writes, not a pulse
        }
        // RESET released: let the host's second line write land, then latch "BOOT0" as the
        // r2 board's NRST rise does.
        Timer::after(LINE_SETTLE).await;
        if control.dtr() && !control.rts() {
            crate::sysboot::reboot_into_bootloader();
        }
        SCB::sys_reset();
    }
}

/// Observes the bus for [`supervisor`]: a host's reset, the configuration, suspend and resume.
struct BusWatch;

impl Handler for BusWatch {
    fn reset(&mut self) {
        CONFIGURED.store(false, Ordering::Relaxed);
        SUSPENDED.store(false, Ordering::Relaxed);
        BUS_EVENT.signal(());
    }

    fn configured(&mut self, configured: bool) {
        CONFIGURED.store(configured, Ordering::Relaxed);
        if configured {
            CONFIGURED_SEEN.store(true, Ordering::Relaxed);
        }
        BUS_EVENT.signal(());
    }

    fn suspended(&mut self, suspended: bool) {
        SUSPENDED.store(suspended, Ordering::Relaxed);
        BUS_EVENT.signal(());
    }
}

/// The console's transmit half over the CDC-ACM IN endpoint.
struct CdcTx {
    sender: Sender<'static, UsbDriver>,
    /// Lead the next write with a lone `0x00`: a stalled write cut a frame short, and the
    /// delimiter makes the host's decoder drop that tail instead of the next frame.
    resync: bool,
}

impl CdcTx {
    /// Hand one packet to the endpoint; `false` if the frame it belongs to should be abandoned
    /// (the endpoint went away, or the host stopped reading). Bounded by [`WRITE_TIMEOUT`] unless
    /// the link is already stalled, in which case it waits for the host to read again.
    async fn packet(&mut self, data: &[u8]) -> bool {
        let stalled = STALLED.load(Ordering::Relaxed);
        let deadline = if stalled {
            Instant::MAX
        } else {
            Instant::now() + WRITE_TIMEOUT
        };
        match with_deadline(deadline, self.sender.write_packet(data)).await {
            Ok(Ok(())) => {
                if stalled {
                    // The host reads again: the console is back up for the producers.
                    STALLED.store(false, Ordering::Relaxed);
                    set_console_up(true);
                }
                true
            }
            Ok(Err(_)) => {
                // The endpoint is disabled (a bus reset, the configuration dropped): park until
                // it is live again; the supervisor ends this console run meanwhile.
                self.sender.wait_connection().await;
                false
            }
            Err(_) => {
                // The host holds the port open but does not read. Producers must not wait on it:
                // drop the console for them, empty the queue (counted as dropped) and resync the
                // host's decoder once it reads again. The cancelled write left nothing queued.
                STALLED.store(true, Ordering::Relaxed);
                self.resync = true;
                set_console_up(false);
                drain_queue();
                false
            }
        }
    }
}

impl LinkTx for CdcTx {
    async fn write(&mut self, bytes: &[u8]) {
        const MPS: usize = MAX_PACKET as usize;
        if core::mem::take(&mut self.resync) && !self.packet(&[0]).await {
            return;
        }
        // Full packets, then the remainder; a transfer that ends on a full packet gets a
        // zero-length one, or the host holds the tail back until more data arrives. One packet
        // write site keeps this future (part of the console manager's static task) small.
        let mut rest = bytes;
        loop {
            let n = rest.len().min(MPS);
            if !self.packet(&rest[..n]).await || n < MPS {
                return;
            }
            rest = &rest[n..];
        }
    }

    async fn drain(&mut self) {
        // Nothing to wait for: a packet sits in the endpoint's buffer until the host takes it,
        // and the session keeps the clocks running (no STOP while USB runs).
    }
}

/// The console's receive half over the CDC-ACM OUT endpoint.
struct CdcRx {
    receiver: Receiver<'static, UsbDriver>,
}

impl LinkRx for CdcRx {
    async fn read(&mut self, buf: &mut [u8]) -> Option<usize> {
        match self.receiver.read_packet(buf).await {
            Ok(n) => Some(n),
            Err(_) => {
                self.receiver.wait_connection().await;
                None
            }
        }
    }
}

/// The host probe: `true` when a USB host is on the cable. The USB transceiver is powered up with
/// every interrupt masked, its own D+ pull-up (1.5 kΩ) is switched on for [`PROBE_PULLUP_CYCLES`],
/// the line state is read back from `USB_FNR.RXDP` (the transceiver's single-ended D+ receiver),
/// and everything is switched off again (see the module docs for the physics). The pull-up is on
/// for under 2.5 µs, the shortest connect a hub registers (USB 2.0, 7.1.7.3, TDCNN), so a host
/// never sees the probe as an attach, and with no VBUS the clamp takes ~2 mA from it for that
/// long only. The clock is switched directly, not through embassy (no STOP refcount for a probe).
fn host_probe() -> bool {
    let regs = embassy_stm32::pac::USB;
    RCC.apb1enr().modify(|w| w.set_usben(true));
    // Transceiver powered, peripheral out of reset, no interrupt enabled.
    regs.cntr().write(|w| w.set_pdwn(false));
    cortex_m::asm::delay(PROBE_STARTUP_CYCLES);
    let high = critical_section::with(|_| {
        regs.bcdr().write(|w| w.set_dppu(true));
        cortex_m::asm::delay(PROBE_PULLUP_CYCLES);
        let fnr = regs.fnr().read();
        regs.bcdr().write(|w| w.set_dppu(false));
        fnr.rxdp()
    });
    regs.cntr().write(|w| {
        w.set_pdwn(true);
        w.set_fres(true);
    });
    regs.istr().write_value(embassy_stm32::pac::usb::regs::Istr(0));
    RCC.apb1enr().modify(|w| w.set_usben(false));
    high
}

/// Park D- and D+ as analog pins (no input buffer, no pull): their idle state, and the one the
/// USB transceiver works in (the L0 USB pins need no alternate function).
fn park_pins() {
    GPIOA.moder().modify(|w| {
        w.set_moder(DM, Moder::ANALOG);
        w.set_moder(DP, Moder::ANALOG);
    });
    GPIOA.pupdr().modify(|w| {
        w.set_pupdr(DM, Pupdr::FLOATING);
        w.set_pupdr(DP, Pupdr::FLOATING);
    });
}

/// Start-up cleanup: the D+ pull-up must be off until a probe has seen a host. After a reset it is
/// (and an application the ROM bootloader's DFU "leave" jumped to restarts through a reset
/// first, see [`sysboot`](crate::sysboot)), so this is belt and braces: a peripheral reset clears
/// the pull-up (`USB_BCDR`) and powers the transceiver down. The clock is touched directly, not
/// through embassy, so no STOP refcount is taken.
fn quiesce() {
    RCC.apb1enr().modify(|w| w.set_usben(true));
    RCC.apb1rstr().modify(|w| w.set_usbrst(true));
    RCC.apb1rstr().modify(|w| w.set_usbrst(false));
    RCC.apb1enr().modify(|w| w.set_usben(false));
    interrupt::USB.disable();
    interrupt::USB.unpend();
    park_pins();
}

/// Power the USB peripheral down after a session: pull-up off (the host sees the detach at once),
/// transceiver powered down, interrupt off, clock off. `rcc::disable` also releases the STOP
/// refcount `Driver::new` took when it enabled the clock (embassy's driver has no `Drop`).
fn power_down() {
    let regs = embassy_stm32::pac::USB;
    regs.bcdr().write(|w| w.set_dppu(false));
    regs.cntr().write(|w| {
        w.set_pdwn(true);
        w.set_fres(true);
    });
    interrupt::USB.disable();
    embassy_stm32::rcc::disable::<USB>();
    interrupt::USB.unpend();
    park_pins();
}

/// Trim HSI48 to the host's SOF (CRS), as USB requires. Started by hand per session, not through
/// embassy's `rcc.hsi48.sync_from_usb`: that would enable the CRS with a STOP refcount at every
/// clock init (boot and each STOP exit) and keep the board out of STOP for good.
fn crs_start() {
    RCC.apb1enr().modify(|w| w.set_crsen(true));
    CRS.cfgr().modify(|w| w.set_syncsrc(Syncsrc::USB));
    CRS.cr().modify(|w| {
        w.set_autotrimen(true);
        w.set_cen(true);
    });
}

/// Stop the CRS after a session ([`crs_start`]).
fn crs_stop() {
    CRS.cr().modify(|w| {
        w.set_cen(false);
        w.set_autotrimen(false);
    });
    RCC.apb1enr().modify(|w| w.set_crsen(false));
}

/// The console's USB serial number, [`SERIAL_PREFIX`] + the unique device id in hex.
fn serial_number() -> &'static str {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    // SAFETY: only [`session`] calls this, one session at a time, before the device that borrows
    // the string exists.
    let buf = unsafe { &mut *addr_of_mut!(SERIAL) };
    buf[..SERIAL_PREFIX.len()].copy_from_slice(SERIAL_PREFIX.as_bytes());
    let digits = &mut buf[SERIAL_PREFIX.len()..];
    for (i, word) in crate::board::unique_id_words().into_iter().enumerate() {
        for nibble in 0..8 {
            digits[i * 8 + nibble] = HEX[(word >> (28 - 4 * nibble)) as usize & 0xF];
        }
    }
    core::str::from_utf8(buf).unwrap_or(SERIAL_PREFIX)
}
