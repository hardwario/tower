//! The taf runtime: one loop that owns the radio for a standard TOWER app.
//!
//! Ported from the `radio_push_button` product app (2026-11-09), whose design notes it keeps: the
//! provisional pairing key and its commit round, the shell-epoch dedup of remote commands, the
//! uplink backoff, the US 915 hop-schedule handling. What was the button app's own (its events,
//! its sensors) is the app's task now; everything else is here, the same for every app.

use core::sync::atomic::{AtomicU8, Ordering};

use embassy_futures::select::{Either4, select4};
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use tower_protocol::mgmt::{self, DeviceInfo, DeviceRole, Joined, MgmtOp, ProvisionAck};
use tower_protocol::msg::MgmtRequest;
use tower_protocol::props::{self, Access, Class, Kind};
use tower_protocol::radio::{
    MAX_RADIO_PAYLOAD, MAX_REPORT, NodeCmd, NodeInfo, NodeMsg, NodeShellChunk, RADIO_SHELL_CHUNK, RADIO_TAG,
    decode_node_cmd, encode_node_msg,
};
use tower_protocol::{MsgType, decode_frame};

use super::{Base, INBOX, PAIR_REQUEST, WAKE};
use crate::board::UniqueId;
use crate::radio::Spirit1;
use crate::radio::config::{self, Band};
use crate::radio::net::{
    Access as NetAccess, COMMIT_SETTLE_MS, CommitStep, FhssConfig, FhssRole, FhssState, Net, NetConfig,
    PAIRING_WINDOW, SendResult, commit_step, gateway_network_key, is_reserved_addr, peer_addr_valid,
};
use crate::shell::{self, Setting, ShellSink};
use crate::storage::{NS_APP, NS_SHELL, Nv};
use crate::{button, console, led, watchdog};

/// The provision record in `NS_APP` (postcard): the gateway, its link key, band and channel. The
/// node's own address is the shell's `addr` setting.
const KEY_PROVISION: u8 = 0x00;

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy)]
struct Provisioned {
    gw_addr: u32,
    key: [u8; 16],
    band: u8,
    channel: u8,
}

static LED_CH: led::LedChannel = led::LedChannel::new();
static BTN_CH: button::ButtonChannel = button::ButtonChannel::new();
/// Double-blink background: not paired with any gateway yet.
static UNPROVISIONED: led::Pattern = &[
    led::Step::on(60),
    led::Step::off(120),
    led::Step::on(60),
    led::Step::off(1760),
];
/// Fast blink while an OTA join runs.
static JOINING: led::Pattern = &[led::Step::on(100), led::Step::off(150)];
/// Slow blink: the radio failed to start.
static RADIO_FAILED: led::Pattern = &[led::Step::on(500), led::Step::off(500)];
/// How long the unpaired double-blink runs (it costs LED current on a node that may sit unpaired
/// for months); a press or a cable request shows it again.
const UNPAIRED_CUE: Duration = Duration::from_secs(180);
/// Post-uplink receive window when the gateway's ACK advertises a queued downlink: the gateway
/// transmits ~20 ms after the uplink with one retry (~450 ms worst); 1 s covers it.
const DOWNLINK_WINDOW: Duration = Duration::from_secs(1);
/// Remote-shell reply chunks go up with extra reps: the gateway may be busy retrying a downlink.
const CHUNK_REPS: u8 = 5;
/// Reps of an event or report uplink (the gesture events of the button app used 5; 3 is enough
/// for the rest, and costs less airtime).
const UPLINK_REPS: u8 = 3;
/// The power-up gesture (docs/firmware/taf.md): the LED flashes once when the runtime is up, and
/// a press within `GESTURE_START` of it starts the gesture, whose stages are `GESTURE_STAGE` long:
/// held 5 s the LED blinks (released: nothing), at 10 s it goes off (released within 5 s: OTA
/// pairing), at 15 s it blinks fast (released: nothing), at 20 s it goes off (released within 5 s:
/// factory reset), at 25 s the button counts as stuck. It cannot start *during* power-up: the
/// button is the BOOT button, and BOOT0 high at reset starts the ROM bootloader instead.
const GESTURE_START: Duration = Duration::from_secs(3);
const GESTURE_STAGE: Duration = Duration::from_secs(5);
const BOOT_FLASH_MS: u32 = 200;
/// The gesture's first blink (pairing next) and its second (factory reset next).
static GESTURE_PAIR: led::Pattern = &[led::Step::on(250), led::Step::off(250)];
static GESTURE_RESET: led::Pattern = &[led::Step::on(50), led::Step::off(50)];
/// An undelivered `Info` is retried this long after, doubling per `Info` lost in a row (never
/// later than the heartbeat's own backoff): the host needs it soon (the node's kind after its own
/// restart, a time sync), not a heartbeat (15 min) later.
const INFO_RETRY: Duration = Duration::from_secs(30);
/// Unacknowledged uplinks in a row after which the node hints that its gateway may have forgotten
/// it (once per streak).
const UNACKED_HINT: u8 = 8;
static UNACKED: AtomicU8 = AtomicU8::new(0);
/// A listening node's receive slice: the longest a local event (a button, a report, the cable)
/// waits for the radio loop. A receive is never cancelled mid-frame (that loses the frame: the
/// replay lane has advanced), so the loop polls everything else between slices.
const LISTEN_SLICE: Duration = Duration::from_millis(50);
/// Hop-slot ticks one send may spend scanning for the schedule (US 915): ~28 s.
const FHSS_SCAN_TICKS: u16 = 80;
/// Slots in a row a synced send may miss the beacon before it gives up.
const FHSS_MISSED_BEACONS: u8 = 8;
/// A hopping node re-anchors this long after its last beacon with nothing to send.
const FHSS_RESYNC: Duration = Duration::from_secs(900);

/// A node's peer table: its gateway, and a new one during a re-pair (the old pairing stays in force
/// until the new one commits). The full table (16) is a gateway's: 448 B of RAM a node never uses.
const NODE_PEERS: usize = 2;

/// Everything the loop carries.
struct Node {
    net: Net<NODE_PEERS>,
    btn: Option<button::Button>,
    led: led::Led,
    kind: &'static Kind,
    kv: Nv,
    addr: u32,
    band: Band,
    channel: u8,
    provision: Option<Provisioned>,
    /// Consecutive undelivered uplinks: the heartbeat backs off on them.
    misses: u8,
    /// Consecutive undelivered `Info`s: the retry backs off on them ([`INFO_RETRY`]).
    info_lost: u8,
    /// After an undelivered uplink, periodic reports wait until then: `report-period` doubled per
    /// miss in a row, at most the heartbeat (an undelivered value stays due, so without this every
    /// new sample would retry it, a confirmed uplink a period forever for a gateway that is gone).
    reports_after: Instant,
    next_heartbeat: Instant,
    cue_until: Option<Instant>,
    /// The power-up gesture: a press may start it until then.
    gesture_open: Instant,
    /// The press that started the power-up gesture, while it is held, and the stage whose LED
    /// it shows (the next one is due a stage after that: computing it from the elapsed time
    /// alone would move it past every wake that should act on it).
    gesture: Option<(Instant, u8)>,
    /// Held through the whole gesture: stuck, ignored until it is released.
    stuck: bool,
    /// A short gesture press ended: the button driver's `Click` right after its `Release` is the
    /// gesture's too, not an event.
    gesture_click: bool,
    /// The gestures since boot (press, release, click, hold), for the log.
    presses: [u32; 4],
    resync_after: Instant,
    /// Running counts of the app's events since boot, by id.
    counts: heapless::Vec<(u8, u32), 8>,
    /// A `Describe` asked for the `Info` and a full report (sent by the loop, not from inside the
    /// downlink window that received it).
    describe_due: bool,
    /// The full report an `Info` was followed by waits for the downlinks its ACK announced.
    report_due: bool,
    /// The last uplink's ACK said a downlink waits: the loop opens the receive window as soon as
    /// the uplink returns (the gateway sends ~20 ms after its ACK), and senders stop until then. A
    /// window served from inside the uplink stacked the remote shell's future on top of every
    /// sender's (RAM: the runtime's future is the app's largest static).
    downlink_due: bool,
    /// An OTA pairing to run from the loop: its window, and the cable request (`JoinOpen`) waiting
    /// for its outcome. Run at the loop's top level, not inside whatever asked for it (RAM, as for
    /// [`downlink_due`](Self::downlink_due)).
    pair_due: Option<(Duration, Option<u16>)>,
}

/// Run the node: never returns. Call it last from the app's `run`, after spawning the app's own
/// task(s); see the module docs.
pub async fn run(base: Base, kind: &'static Kind, settings: &'static [Setting]) -> ! {
    // Built in its own future: a local `Net` moved into the `Node` here would keep its storage for
    // the life of the loop below (measured: 1 KB of this app's RAM twice).
    let mut node = start(base, kind, settings).await;
    if kind.class == Class::Listening {
        node.listen().await
    } else {
        node.sleep_loop().await
    }
}

/// Bring the node up: watchdog, LED, button, radio, the provisioned gateway.
async fn start(base: Base, kind: &'static Kind, settings: &'static [Setting]) -> Node {
    // A wedged unit resets itself instead of dying in the field (26 s: the longest nominal
    // timeout, so the feeder's wakes stay rare on a battery node).
    watchdog::enable(base.iwdg, base.spawner, Duration::from_secs(26));
    let led = led::init(
        base.spawner,
        Output::new(base.led, Level::Low, Speed::Low),
        &LED_CH,
        led::Polarity::ActiveHigh,
    );
    let kv = base.kv;
    let addr = shell::radio_addr(kv);
    super::init_shared(kind, settings, kv, led, addr);
    base.spawner
        .spawn(super::sensors::task(base.accel_int, base.i2c).unwrap());
    let btn = base.button.map(|pin| {
        let hold = super::param(props::common::HOLD_TIME).clamp(100, 10_000) as u64;
        let click = super::param(props::common::CLICK_TIMEOUT).clamp(50, 5000) as u64;
        let config = button::Config {
            hold_time: Duration::from_millis(hold),
            click_timeout: Duration::from_millis(click),
            debounce_press: Duration::from_millis(30),
            debounce_release: Duration::from_millis(30),
            ..button::Config::default()
        };
        button::init_exti(base.spawner, pin, button::Polarity::ActiveHigh, config, &BTN_CH)
    });

    let mut provision: Option<Provisioned> =
        kv.scope(NS_APP).get::<Provisioned>(KEY_PROVISION).ok().flatten();
    if kind.class == Class::Listening && provision.is_some_and(|p| p.band == mgmt::BAND_US915) {
        // A board paired on US 915 under a sleeping app, reflashed with a listening one: that
        // pairing cannot be used (see below), so the node starts unpaired on the `band` setting,
        // and pairing again on EU 868 replaces the record.
        log::error!(target: "node", "pairing on US 915 unused (listening: EU 868 only), pair again");
        provision = None;
    }
    let (band, channel) = match provision {
        Some(p) => (
            if p.band == mgmt::BAND_US915 {
                Band::Us915
            } else {
                Band::Eu868
            },
            p.channel,
        ),
        None => (
            if super::param(props::common::BAND) == 1 {
                Band::Us915
            } else {
                Band::Eu868
            },
            0,
        ),
    };
    let r = base.radio;
    let radio = Spirit1::new(r.spi, r.sck, r.mosi, r.miso, r.cs, r.sdn, r.irq);
    let net = Net::<NODE_PEERS>::with_peers(
        radio,
        kv,
        NetConfig {
            addr,
            // Not a link key: the node runs registered peers only, so the default key never seals
            // or opens anything (see `set_default_lane` below).
            key: [0u8; 16],
            band,
            channel,
            // A provision record means this store held a TX watermark: fail closed if it is gone.
            keys_stored: provision.is_some(),
        },
    )
    .await;
    let mut net = match net {
        Ok(n) => n,
        Err(e) => {
            log::error!(target: "node", "radio init failed ({e}), running without radio");
            led.set_background(Some(RADIO_FAILED));
            loop {
                Timer::after_secs(3600).await;
            }
        }
    };
    net.set_default_lane(false);
    let listening = kind.class == Class::Listening;
    // Persist the gateway's lane on every accepted downlink, both classes. A lane that lags (the
    // gateway's period of 32) lets recorded downlinks replay into a rebooted node, and a recorded
    // `system reboot` would then reboot it again after every reboot, a loop (the remote-shell dedup
    // is RAM only). The wear is one small record per command: a relay switched a thousand times a
    // day compacts the store about ten times a day (docs/security-model.md, docs/firmware/storage.md).
    net.set_lane_persist_period(1);
    if let Some(p) = provision
        && !net.add_peer(p.gw_addr, &p.key)
    {
        log::error!(
            target: "node",
            "gateway 0x{:08x} is not a valid peer of 0x{:08x}, radio off: fix addr or re-pair",
            p.gw_addr,
            addr
        );
        provision = None;
    }
    if listening && band == Band::Us915 {
        // A listening node would have to follow the hop schedule continuously, which is not done
        // yet (TASKS.md, T3). Anything else on US 915 is one fixed channel at full power, which FCC
        // §15.247 does not allow outside the bench (docs/firmware/radio.md): no radio at all instead.
        log::error!(target: "node", "a listening node runs on EU 868 only for now, radio off (set band eu868)");
        provision = None;
    } else if let Some(p) = provision
        && band == Band::Us915
    {
        start_hopping(&mut net, p.gw_addr).await;
    }
    if !listening {
        let _ = net.sleep().await;
    }

    let mut node = Node {
        net,
        btn,
        led,
        kind,
        kv,
        addr,
        band,
        channel,
        provision,
        misses: 0,
        info_lost: 0,
        reports_after: Instant::MIN,
        next_heartbeat: Instant::now(),
        cue_until: None,
        gesture_open: Instant::MIN,
        gesture: None,
        stuck: false,
        gesture_click: false,
        presses: [0; 4],
        resync_after: Instant::MIN,
        counts: heapless::Vec::new(),
        describe_due: false,
        report_due: false,
        downlink_due: false,
        pair_due: None,
    };
    node.paired_changed();
    match node.provision {
        Some(p) => info!(target: "node", "0x{:08x} paired with gateway 0x{:08x}", addr, p.gw_addr),
        None => {
            info!(target: "node", "0x{:08x} not paired: power up, hold the button 10 s (or `node pair`)", addr);
            node.show_unpaired();
        }
    }
    // The runtime is up: one flash, and the power-up gesture may start (a press already held when
    // the runtime came up starts it too).
    node.led.flash(BOOT_FLASH_MS);
    node.gesture_open = Instant::now() + GESTURE_START;
    if node.btn.is_some_and(|b| b.is_pressed()) {
        node.start_gesture();
    }
    node
}

impl Node {
    fn gw(&self) -> Option<u32> {
        self.provision.map(|p| p.gw_addr)
    }

    fn paired_changed(&mut self) {
        super::GATEWAY.store(self.gw().unwrap_or(0), Ordering::Relaxed);
    }

    fn show_unpaired(&mut self) {
        self.led.set_background(Some(UNPROVISIONED));
        self.cue_until = Some(Instant::now() + UNPAIRED_CUE);
    }

    fn heartbeat_period(&self) -> Duration {
        Duration::from_secs(super::param(props::common::HEARTBEAT).clamp(60, 86_400) as u64)
    }

    /// The sleeping node's loop: the button, the timers, the app's reports and events, the cable.
    async fn sleep_loop(&mut self) -> ! {
        loop {
            let gesture_due = self.gesture_due();
            let resync_due = self
                .net
                .fhss_since_beacon()
                .filter(|_| self.net.access() == NetAccess::Fhss)
                .map(|since| {
                    let rest = FHSS_RESYNC.checked_sub(since).unwrap_or(Duration::from_ticks(0));
                    (Instant::now() + rest).max(self.resync_after)
                });
            let due = self
                .next_heartbeat
                .min(gesture_due.unwrap_or(Instant::MAX))
                .min(self.cue_until.unwrap_or(Instant::MAX))
                .min(resync_due.unwrap_or(Instant::MAX));
            let btn = self.btn;
            let press = async {
                match btn {
                    Some(b) => b.next_event().await,
                    None => core::future::pending().await,
                }
            };
            match select4(press, Timer::at(due), WAKE.wait(), console::mgmt_next()).await {
                Either4::First(ev) => self.on_button(ev).await,
                Either4::Second(()) => {
                    if resync_due.is_some_and(|t| Instant::now() >= t) && !fhss_resync(&mut self.net).await {
                        self.resync_after = Instant::now() + Duration::from_secs(60);
                    }
                    self.on_timer().await;
                }
                Either4::Third(()) => self.on_wake().await,
                Either4::Fourth(frame) => {
                    let cable = Cable::parse(&frame);
                    self.on_cable(cable).await;
                }
            }
            self.serve_downlinks().await;
            self.run_pairing().await;
        }
    }

    /// The listening node's loop: receive slices, never cancelled; everything else between them.
    async fn listen(&mut self) -> ! {
        loop {
            if let Some(rx) = self.net.recv(LISTEN_SLICE).await
                && Some(rx.src) == self.gw()
            {
                let mut data = [0u8; MAX_RADIO_PAYLOAD];
                let n = rx.data().len().min(MAX_RADIO_PAYLOAD);
                data[..n].copy_from_slice(&rx.data()[..n]);
                self.on_downlink(&data[..n]).await;
            }
            if let Some(ev) = self.btn.and_then(|b| b.try_event()) {
                self.on_button(ev).await;
            }
            // The timers before the app's reports, as in the sleeping loop: at boot the `Info` goes
            // first and the app's first values ride in its full report. A `Report` ahead of the
            // `Info` would make a host that does not know the kind yet ask for a `Describe`, and
            // then answer two `Info`s (two time syncs, two full reports).
            let now = Instant::now();
            if now >= self.next_heartbeat
                || self.gesture_due().is_some_and(|t| now >= t)
                || self.cue_until.is_some_and(|t| now >= t)
            {
                self.on_timer().await;
            }
            if WAKE.signaled() {
                WAKE.reset();
                self.on_wake().await;
            }
            if let Some(frame) = console::mgmt_try_next() {
                let cable = Cable::parse(&frame);
                self.on_cable(cable).await;
            }
            self.run_pairing().await;
        }
    }

    async fn on_button(&mut self, ev: button::Event) {
        if self.stuck {
            // Held through the whole gesture: nothing until it is released.
            self.stuck = ev != button::Event::Release;
            return;
        }
        if core::mem::take(&mut self.gesture_click) && ev == button::Event::Click {
            return;
        }
        if self.gesture.is_some() {
            // The gesture's own press, hold and release are not events.
            if ev == button::Event::Release {
                self.end_gesture().await;
            }
            return;
        }
        if ev == button::Event::Press && Instant::now() < self.gesture_open {
            self.start_gesture();
            return;
        }
        if self.provision.is_none() {
            // Someone is at the node: show the unpaired cue again.
            self.show_unpaired();
        } else if self.misses > 0 {
            // Someone is at the node: end the backoff.
            self.misses = 0;
            self.reports_after = Instant::MIN;
        }
        super::button_event(ev, &mut self.presses);
        let _ = super::BUTTON.try_send(ev);
    }

    // ---- the power-up gesture -------------------------------------------------------------------

    fn start_gesture(&mut self) {
        self.gesture = Some((Instant::now(), 0));
        self.cue_until = None;
        self.led.set_background(None);
    }

    /// The gesture's stage: whole `GESTURE_STAGE`s held so far.
    fn gesture_stage(&self) -> Option<u64> {
        self.gesture
            .map(|(t, _)| t.elapsed().as_millis() / GESTURE_STAGE.as_millis())
    }

    /// When the held gesture's next stage is due (after the one its LED shows).
    fn gesture_due(&self) -> Option<Instant> {
        let (t, shown) = self.gesture?;
        Some(t + GESTURE_STAGE * (u32::from(shown) + 1))
    }

    /// The held gesture entered a stage: its LED, or the stuck verdict.
    fn gesture_tick(&mut self) {
        let (Some((t, _)), Some(stage)) = (self.gesture, self.gesture_stage()) else {
            return;
        };
        if !self.btn.is_some_and(|b| b.is_pressed()) {
            // Released, but the Release was lost to a full queue: no gesture.
            self.gesture = None;
            self.restore_cue();
            return;
        }
        match stage {
            0 => {}
            1 => self.led.set_background(Some(GESTURE_PAIR)),
            3 => self.led.set_background(Some(GESTURE_RESET)),
            2 | 4 => self.led.set_background(None),
            _ => {
                warn!(target: "button", "held over 25 s: stuck, ignored until released");
                self.gesture = None;
                self.stuck = true;
                self.restore_cue();
                return;
            }
        }
        self.gesture = Some((t, stage as u8));
    }

    /// The gesture's button came up: pairing after 10 to 15 s, the factory reset after 20 to 25 s,
    /// nothing otherwise.
    async fn end_gesture(&mut self) {
        let stage = self.gesture_stage();
        self.gesture = None;
        self.gesture_click = true;
        self.led.set_background(None);
        match stage {
            Some(2) => {
                info!(target: "node", "pairing (button released after 10 s)");
                self.pair_due = Some((PAIRING_WINDOW, None));
            }
            Some(4) => {
                warn!(target: "node", "factory reset (button released after 20 s), rebooting");
                if self.kv.wipe().is_err() {
                    log::error!(target: "storage", "wipe failed (the store is whole or empty)");
                }
                // The store is frozen now: reboot, as the shell's `system eeprom wipe` does.
                console::flush().await;
                cortex_m::peripheral::SCB::sys_reset();
            }
            _ => self.restore_cue(),
        }
    }

    /// Back from the gesture: the unpaired cue again when unpaired.
    fn restore_cue(&mut self) {
        if self.provision.is_none() {
            self.show_unpaired();
        }
    }

    async fn on_timer(&mut self) {
        let now = Instant::now();
        if self.cue_until.is_some_and(|t| now >= t) {
            self.cue_until = None;
            self.led.set_background(None);
        }
        // After the cue, so the gesture's LED wins.
        if self.gesture_due().is_some_and(|t| now >= t) {
            self.gesture_tick();
        }
        if now >= self.next_heartbeat {
            // The period, doubled per undelivered uplink in a row, capped at a day.
            let period = self.heartbeat_period().as_secs();
            let wait = tower_radio::uplink_backoff_s(period, 86_400, self.misses);
            self.next_heartbeat = now + Duration::from_secs(wait);
            if self.provision.is_some() {
                let ok = self.send_info().await;
                self.note(ok);
                if ok {
                    self.full_report_after_info().await;
                }
            }
        }
    }

    /// The app reported or raised something, or asked for pairing.
    async fn on_wake(&mut self) {
        // One writer clears it (this loop): a load and a store are enough on the M0+.
        if PAIR_REQUEST.load(Ordering::Relaxed) {
            PAIR_REQUEST.store(false, Ordering::Relaxed);
            self.pair_due = Some((PAIRING_WINDOW, None));
        }
        if self.describe_due && self.provision.is_some() {
            self.describe_due = false;
            // Its full report stands for a pending one too (one full report per wake).
            self.report_due = false;
            if self.send_info().await {
                self.full_report_after_info().await;
            }
        }
        if self.report_due && !self.downlink_due && self.provision.is_some() {
            self.report_due = false;
            self.send_reports(true).await;
        }
        while !self.downlink_due
            && let Ok((id, value)) = super::EVENTS.try_receive()
        {
            let count = self.count(id);
            if self.provision.is_none() {
                continue;
            }
            let ok = self
                .uplink(&NodeMsg::Event { id, value, count }, UPLINK_REPS)
                .await;
            self.note(ok);
        }
        if self.provision.is_some() && !self.downlink_due {
            self.send_reports(false).await;
        }
        if self.downlink_due {
            // What is left goes after the downlinks.
            WAKE.signal(());
        }
    }

    fn count(&mut self, id: u8) -> u32 {
        match self.counts.iter_mut().find(|(i, _)| *i == id) {
            Some((_, c)) => {
                *c = c.wrapping_add(1);
                *c
            }
            None => {
                let _ = self.counts.push((id, 1));
                1
            }
        }
    }

    /// Send the reported values that are due (all of them when `full`), and every setting when
    /// `full` or when one changed behind the runtime's back (the USB console), one `Report` of up
    /// to [`MAX_REPORT`] at a time: built per uplink, so no list of every value is held across the
    /// uplink's awaits (RAM: the runtime's future is the app's largest static).
    async fn send_reports(&mut self, full: bool) {
        if !full && Instant::now() < self.reports_after {
            // Backing off after a miss: what is due waits (a heartbeat's full report still goes).
            return;
        }
        let now_s = Instant::now().as_secs();
        let mut from = Some(0);
        while let Some(at) = from
            && !self.downlink_due
        {
            let mut chunk = heapless::Vec::new();
            from = super::due_reports(full, now_s, at, &mut chunk);
            if chunk.is_empty() {
                break;
            }
            if !self.send_report(chunk, now_s).await {
                return;
            }
        }
        if self.downlink_due {
            // An ACK announced a downlink: the settings go after it (a full report's states went,
            // or are no news to the policy and wait for the next heartbeat).
            if full {
                super::PARAMS_DUE.store(true, Ordering::Relaxed);
            }
            WAKE.signal(());
            return;
        }
        // One writer clears it (this loop): a load and a store are enough on the M0+. A setting
        // report that fails is not retried: the next heartbeat carries them all.
        if !(full || super::PARAMS_DUE.load(Ordering::Relaxed)) {
            return;
        }
        super::PARAMS_DUE.store(false, Ordering::Relaxed);
        let mut props = self
            .kind
            .all()
            .filter(|p| matches!(p.access, Access::Param | Access::LocalParam));
        loop {
            let mut chunk = heapless::Vec::new();
            for p in props.by_ref() {
                let _ = chunk.push((p.id, super::param(p.id)));
                if chunk.is_full() {
                    break;
                }
            }
            if chunk.is_empty() || !self.send_report(chunk, now_s).await {
                return;
            }
            if self.downlink_due {
                // The rest after the downlink: all the settings again (a chunk or two).
                super::PARAMS_DUE.store(true, Ordering::Relaxed);
                WAKE.signal(());
                return;
            }
        }
    }

    /// One `Report` uplink; on delivery, its values count as sent.
    async fn send_report(&mut self, items: heapless::Vec<(u8, i32), MAX_REPORT>, now_s: u64) -> bool {
        let msg = NodeMsg::Report(items);
        let ok = self.uplink(&msg, UPLINK_REPS).await;
        self.note(ok);
        if ok && let NodeMsg::Report(items) = &msg {
            super::mark_sent(items, now_s);
        }
        ok
    }

    fn note(&mut self, delivered: bool) {
        if delivered {
            self.misses = 0;
            self.reports_after = Instant::MIN;
        } else {
            self.misses = self.misses.saturating_add(1);
            let period = super::param(props::common::REPORT_PERIOD).clamp(5, 86_400) as u64;
            let cap = self.heartbeat_period().as_secs();
            let wait = tower_radio::uplink_backoff_s(period, cap, self.misses);
            self.reports_after = Instant::now() + Duration::from_secs(wait);
        }
    }

    // ---- uplink and downlink ------------------------------------------------------------------

    fn info(&self) -> NodeInfo<'static> {
        NodeInfo {
            firmware_name: "",
            firmware_version: console::firmware_version(),
            session_id: console::session_id(),
            sleeping: self.kind.class == Class::Sleeping,
            wants_time: super::wants_time(self.kind),
        }
    }

    async fn send_info(&mut self) -> bool {
        let name = console::firmware_name();
        let info = NodeInfo {
            firmware_name: name.as_str(),
            ..self.info()
        };
        let ok = self.uplink(&NodeMsg::Info(info), UPLINK_REPS).await;
        if ok {
            self.info_lost = 0;
        } else {
            // Retried by the heartbeat timer, brought forward (a later heartbeat stays later).
            let retry = tower_radio::uplink_backoff_s(INFO_RETRY.as_secs(), 86_400, self.info_lost);
            self.info_lost = self.info_lost.saturating_add(1);
            self.next_heartbeat = self
                .next_heartbeat
                .min(Instant::now() + Duration::from_secs(retry));
        }
        ok
    }

    /// After an `Info` (a heartbeat, a `Describe`): the full report, unless a downlink came first
    /// (then after it).
    async fn full_report_after_info(&mut self) {
        if self.downlink_due {
            // Not the `Info` again: a second one would ask the host for the time twice.
            self.report_due = true;
            WAKE.signal(());
        } else {
            self.send_reports(true).await;
        }
    }

    /// One confirmed uplink. On a sleeping node the radio sleeps after it, unless its ACK says a
    /// downlink waits: then the radio stays up and [`downlink_due`](Self::downlink_due) is set, and
    /// the caller returns to the loop, which serves it at once.
    async fn uplink(&mut self, msg: &NodeMsg<'_>, reps: u8) -> bool {
        let Some(gw) = self.gw() else { return false };
        let listening = self.kind.class == Class::Listening;
        if !listening {
            let _ = self.net.wake().await;
        }
        let result = {
            let mut buf = [0u8; MAX_RADIO_PAYLOAD];
            let Ok(n) = encode_node_msg(msg, &mut buf) else {
                warn!(target: "node", "uplink not encoded");
                return false;
            };
            radio_send(&mut self.net, gw, &buf[..n], reps).await
        };
        let delivered = matches!(result, SendResult::Delivered);
        if delivered {
            UNACKED.store(0, Ordering::Relaxed);
        } else {
            let what = match msg {
                NodeMsg::Info(_) => "info",
                NodeMsg::Report(_) => "report",
                NodeMsg::Event { .. } => "event",
                NodeMsg::Shell(_) => "shell reply",
            };
            warn!(target: "node", "{} uplink {}", what, result);
            let streak = match result {
                SendResult::DutyLimited => UNACKED.load(Ordering::Relaxed),
                _ => UNACKED.load(Ordering::Relaxed).saturating_add(1),
            };
            UNACKED.store(streak, Ordering::Relaxed);
            if streak == UNACKED_HINT && !matches!(result, SendResult::DutyLimited) {
                warn!(
                    target: "node",
                    "gateway 0x{:08x}: {} uplinks unacknowledged; if it forgot this node, pair it again",
                    gw,
                    streak
                );
            }
        }
        if !listening {
            if self.net.last_ack().is_some_and(|m| m.pending) {
                self.downlink_due = true;
            } else {
                let _ = self.net.sleep().await;
            }
        }
        delivered
    }

    /// Serve the downlinks the last uplink's ACK announced: one receive window each, chained while
    /// the replies' ACKs announce more; then the radio sleeps.
    async fn serve_downlinks(&mut self) {
        if !self.downlink_due {
            return;
        }
        self.downlink_due = false;
        let Some(gw) = self.gw() else { return };
        while self.net.last_ack().is_some_and(|m| m.pending) {
            if !self.downlink_window(gw).await {
                break;
            }
        }
        let _ = self.net.sleep().await;
    }

    /// Hold RX open for one queued downlink and handle it; whether one arrived.
    async fn downlink_window(&mut self, gw: u32) -> bool {
        let deadline = Instant::now()
            + if self.net.access() == NetAccess::Fhss {
                self.net.fhss_slot_left()
            } else {
                DOWNLINK_WINDOW
            };
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.as_ticks() == 0 {
                return false;
            }
            // `None` is a timeout, or a frame that is not ours: neither ends the window early.
            let Some(rx) = self.net.recv(remaining).await else {
                continue;
            };
            if rx.src != gw {
                continue;
            }
            let mut data = [0u8; MAX_RADIO_PAYLOAD];
            let n = rx.data().len().min(MAX_RADIO_PAYLOAD);
            data[..n].copy_from_slice(&rx.data()[..n]);
            return self.on_downlink(&data[..n]).await;
        }
    }

    /// One command from the gateway; whether it decoded (a sleeping node keeps chaining).
    async fn on_downlink(&mut self, data: &[u8]) -> bool {
        let Some(gw) = self.gw() else { return false };
        match decode_node_cmd(data) {
            Ok(NodeCmd::Shell { epoch, cmd_id, line }) => {
                remote_shell(&mut self.net, gw, epoch, cmd_id, line).await;
                true
            }
            Ok(NodeCmd::Set { id, value }) => {
                self.on_set(id, value);
                true
            }
            Ok(NodeCmd::Time { unix, utc_offset_min }) => {
                super::sync_clock(unix, utc_offset_min);
                if let Some(t) = super::local_time() {
                    info!(
                        target: "node",
                        "time synced: {} {:02}:{:02}:{:02} (UTC{:+} min)",
                        tower_taf::program::DAY_NAMES[t.weekday as usize],
                        t.hour(),
                        t.minute_of_hour(),
                        t.second,
                        utc_offset_min
                    );
                }
                true
            }
            Ok(NodeCmd::Describe) => {
                self.describe_due = true;
                WAKE.signal(());
                true
            }
            Err(tower_protocol::Error::SchemaMismatch { got }) => {
                warn!(
                    target: "node",
                    "downlink schema {:#06x}, expected {:#06x}: build host and node from one checkout",
                    got,
                    RADIO_TAG
                );
                false
            }
            Err(_) => {
                warn!(target: "node", "downlink malformed, dropped");
                false
            }
        }
    }

    /// A `set` from the host: a setting is stored here, a control goes to the app. Either way the
    /// property is reported back (the confirmation the host waits for) by the loop, after the
    /// downlink chain: an uplink from inside the window that received the command would open a
    /// window of its own. A control's report is made due here and goes up once the app applied it
    /// (its own report of a change, or its next [`next_set`](super::next_set) for no change).
    fn on_set(&mut self, id: u8, value: i32) {
        let Some(prop) = self.kind.prop(id) else {
            warn!(target: "node", "set of unknown property {}", id);
            return;
        };
        if !prop.ty.accepts(value) {
            warn!(target: "node", "set {} refused: {} out of range", prop.name, value);
            return;
        }
        match prop.access {
            Access::Param => {
                if super::set_param(id, value) {
                    info!(target: "node", "set {} = {}", prop.name, prop.ty.display(value));
                    // Confirmed by the settings report (all of them, after the downlinks), not
                    // through a report slot: a setting in a slot would be resent from there,
                    // stale, once the console changed it, and would take a slot a state needs.
                    super::setting_changed();
                    let _ = INBOX.try_send((id, value));
                } else {
                    warn!(target: "node", "set {} not stored", prop.name);
                }
            }
            Access::Control => {
                info!(target: "node", "set {} = {}", prop.name, prop.ty.display(value));
                super::force_report(id);
                if INBOX.try_send((id, value)).is_err() {
                    warn!(target: "node", "set {} dropped (the app is busy)", prop.name);
                }
            }
            _ => warn!(target: "node", "set {} refused: not settable over the radio", prop.name),
        }
    }

    // ---- pairing ------------------------------------------------------------------------------

    /// A listening node on US 915: refused until it can follow the hop schedule (see `start`).
    fn us915_refused(&self) -> bool {
        self.kind.class == Class::Listening && self.band == Band::Us915
    }

    /// OTA pairing: the handshake for `window`, then the commit round; the credentials persist
    /// only once the gateway acknowledged an uplink under the new key. The pairing in force before
    /// stays unless the new one commits.
    async fn pair(&mut self, window: Duration) -> Option<(Provisioned, u32, bool)> {
        if self.us915_refused() {
            log::error!(target: "node", "pairing: a listening node runs on EU 868 only for now (set band eu868)");
            return None;
        }
        let current = self.provision;
        // US 915: pairing is a fixed-channel exchange on the rendezvous carrier at FCC §15.249
        // power, off the hop schedule; the gateway parks there for its window.
        let us915 = self.band == Band::Us915;
        if us915 {
            let _ = self.net.fhss_suspend(Band::Us915).await;
            let _ = self.net.set_band(Band::Us915, 0).await;
            let _ = self.net.set_tx_power(config::PA_INDEX_LOW).await;
        }
        let outcome = self.pair_on_air(window, current).await;
        if us915 {
            let _ = self.net.set_tx_power(config::PA_INDEX_FULL).await;
            match outcome {
                Some((rec, _, _)) => start_hopping(&mut self.net, rec.gw_addr).await,
                None if current.is_some() => self.net.fhss_resume(),
                None => {}
            }
            if self.kind.class == Class::Sleeping {
                let _ = self.net.sleep().await;
            }
        }
        if let Some((rec, _, _)) = outcome {
            self.provision = Some(rec);
            self.paired_changed();
            self.misses = 0;
            self.cue_until = None;
            super::forget_sent();
            // The new gateway has seen nothing: the full picture, now.
            self.next_heartbeat = Instant::now();
        } else if self.provision.is_none() {
            self.show_unpaired();
        }
        outcome
    }

    async fn pair_on_air(
        &mut self,
        window: Duration,
        current: Option<Provisioned>,
    ) -> Option<(Provisioned, u32, bool)> {
        info!(target: "node", "pairing: looking for a gateway window, {} s", window.as_secs());
        self.led.set_background(Some(JOINING));
        let listening = self.kind.class == Class::Listening;
        let _ = self.net.wake().await;
        let joined = self.net.join(self.addr, window).await;
        if !listening {
            let _ = self.net.sleep().await;
        }
        let idle_led = if current.is_some() {
            None
        } else {
            Some(UNPROVISIONED)
        };
        let Some(peer) = joined else {
            warn!(target: "node", "pairing failed: no gateway window in range");
            self.led.set_background(idle_led);
            return None;
        };
        let sas = peer.sas;
        let rec = Provisioned {
            gw_addr: peer.addr,
            key: peer.key,
            band: if self.band == Band::Us915 {
                mgmt::BAND_US915
            } else {
                mgmt::BAND_EU868
            },
            channel: self.channel,
        };
        if !self.net.add_peer(rec.gw_addr, &rec.key) {
            warn!(target: "node", "pairing: gateway 0x{:08x} is not a valid peer, discarded", rec.gw_addr);
            self.led.set_background(idle_led);
            return None;
        }
        Timer::after_millis(COMMIT_SETTLE_MS as u64).await;
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            match commit_step(attempt, self.commit_uplink(rec.gw_addr).await) {
                CommitStep::Commit => break,
                CommitStep::Retry { after_ms } => Timer::after_millis(after_ms as u64).await,
                CommitStep::Discard => {
                    match current {
                        Some(c) if c.gw_addr == rec.gw_addr => {
                            let _ = self.net.add_peer(c.gw_addr, &c.key);
                        }
                        _ => {
                            self.net.remove_peer(rec.gw_addr);
                        }
                    }
                    warn!(
                        target: "node",
                        "pairing: gateway 0x{:08x} did not confirm (no ACK), key discarded{}",
                        rec.gw_addr,
                        if current.is_some() { ", the previous pairing stays" } else { ", try again" }
                    );
                    self.led.set_background(idle_led);
                    return None;
                }
            }
        }
        if let Some(c) = current
            && c.gw_addr != rec.gw_addr
        {
            self.net.remove_peer(c.gw_addr);
        }
        UNACKED.store(0, Ordering::Relaxed);
        let persisted = self.kv.scope(NS_APP).set(KEY_PROVISION, &rec).is_ok();
        if persisted {
            info!(
                target: "node",
                "paired with gateway 0x{:08x}, SAS {:06} (uid {})",
                rec.gw_addr,
                sas,
                UniqueId::read()
            );
        } else {
            warn!(target: "node", "paired with gateway 0x{:08x} but not saved (lost at reboot)", rec.gw_addr);
        }
        self.led.set_background(None);
        Some((rec, sas, persisted))
    }

    /// One commit attempt: the `Info`, confirmed, under the new key. No downlink window.
    async fn commit_uplink(&mut self, gw_addr: u32) -> bool {
        let name = console::firmware_name();
        let info = NodeInfo {
            firmware_name: name.as_str(),
            ..self.info()
        };
        let mut buf = [0u8; MAX_RADIO_PAYLOAD];
        let Ok(n) = encode_node_msg(&NodeMsg::Info(info), &mut buf) else {
            return false;
        };
        let listening = self.kind.class == Class::Listening;
        let _ = self.net.wake().await;
        let result = self.net.send(gw_addr, &buf[..n], true, 3).await;
        if !listening {
            let _ = self.net.sleep().await;
        }
        if !matches!(result, SendResult::Delivered) {
            warn!(target: "node", "pairing: commit uplink {result}");
            return false;
        }
        true
    }

    // ---- the cable (management over USB) ------------------------------------------------------

    /// Run the pairing [`pair_due`](Self::pair_due) asks for, and answer its cable request.
    async fn run_pairing(&mut self) {
        let Some((window, req)) = self.pair_due.take() else {
            return;
        };
        let outcome = self.pair(window).await;
        if let Some(req_id) = req {
            match outcome {
                Some((rec, sas, true)) => {
                    respond_record(
                        req_id,
                        &Joined {
                            gw_addr: rec.gw_addr,
                            sas,
                        },
                    )
                    .await
                }
                Some((_, _, false)) => respond_empty(req_id, mgmt::MGMT_STORAGE).await,
                None => respond_empty(req_id, mgmt::MGMT_TIMEOUT).await,
            }
        }
    }

    async fn on_cable(&mut self, cable: Option<Cable>) {
        let Some(cable) = cable else { return };
        self.handle_mgmt(cable).await;
        self.misses = 0;
        if self.provision.is_some() {
            self.led.set_background(None);
            self.cue_until = None;
        } else {
            self.show_unpaired();
        }
    }

    async fn handle_mgmt(&mut self, cable: Cable) {
        match cable {
            Cable::Describe(req_id) => {
                let name = console::firmware_name();
                respond_record(
                    req_id,
                    &DeviceInfo {
                        role: DeviceRole::Node,
                        addr: self.addr,
                        band: match self.band {
                            Band::Eu868 => mgmt::BAND_EU868,
                            Band::Us915 => mgmt::BAND_US915,
                        },
                        channel: self.channel,
                        node_capacity: 0,
                        node_count: 0,
                        provisioned: self.provision.is_some(),
                        gw_addr: self.gw().unwrap_or(0),
                        firmware_name: name.as_str(),
                        firmware_version: console::firmware_version(),
                    },
                )
                .await;
            }
            Cable::Provision(req_id, p) => {
                let override_addr = p.addr.filter(|&a| a != 0);
                let effective_addr = override_addr.unwrap_or(self.addr);
                // A listening node runs on EU 868 only for now (see `start`).
                let us915_refused = self.kind.class == Class::Listening && p.band == mgmt::BAND_US915;
                if p.band > mgmt::BAND_US915
                    || us915_refused
                    || p.channel > mgmt::MAX_CHANNEL
                    || is_reserved_addr(effective_addr)
                    || !peer_addr_valid(p.gw_addr, effective_addr)
                {
                    respond_empty(req_id, mgmt::MGMT_BAD_ARG).await;
                    return;
                }
                // The key is minted for this provision (it rides this board's TX counter from
                // wherever it stands; see docs/security-model.md, 4.1).
                let rec = Provisioned {
                    gw_addr: p.gw_addr,
                    key: p.key,
                    band: p.band,
                    channel: p.channel,
                };
                // The address pin first: the provision record below is the single commit point.
                if let Some(override_addr) = override_addr
                    && self
                        .kv
                        .scope(NS_SHELL)
                        .set_bytes(shell::ADDR_KEY, &override_addr.to_le_bytes())
                        .is_err()
                {
                    respond_empty(req_id, mgmt::MGMT_STORAGE).await;
                    return;
                }
                if self.kv.scope(NS_APP).set(KEY_PROVISION, &rec).is_err() {
                    respond_empty(req_id, mgmt::MGMT_STORAGE).await;
                    return;
                }
                respond_record(req_id, &ProvisionAck { addr: effective_addr }).await;
                info!(target: "node", "provisioned for gateway 0x{:08x}, rebooting", p.gw_addr);
                console::flush().await;
                cortex_m::peripheral::SCB::sys_reset();
            }
            Cable::JoinOpen(req_id, window) => {
                if window == 0 || self.us915_refused() {
                    respond_empty(req_id, mgmt::MGMT_BAD_ARG).await;
                    return;
                }
                // The same pairing as the button's, commit round included; answered when it ends.
                self.pair_due = Some((Duration::from_secs(window as u64), Some(req_id)));
            }
            Cable::Other(req_id) => respond_empty(req_id, mgmt::MGMT_UNSUPPORTED).await,
        }
    }
}

/// A management request from the cable, decoded out of its frame (which is dropped before
/// anything awaits: RAM).
enum Cable {
    Describe(u16),
    Provision(u16, mgmt::Provision),
    JoinOpen(u16, u16),
    Other(u16),
}

impl Cable {
    fn parse(frame: &[u8]) -> Option<Self> {
        let Ok((MsgType::MgmtRequest, _seq, payload)) = decode_frame(frame) else {
            return None;
        };
        let req = postcard::from_bytes::<MgmtRequest>(payload).ok()?;
        Some(match req.op {
            MgmtOp::Describe => Cable::Describe(req.req_id),
            MgmtOp::Provision(p) => Cable::Provision(req.req_id, p),
            MgmtOp::JoinOpen { window } => Cable::JoinOpen(req.req_id, window),
            _ => Cable::Other(req.req_id),
        })
    }
}

async fn respond_empty(req_id: u16, result: u8) {
    console::mgmt_chunk(req_id, result, 0, true, &[]).await;
}

async fn respond_record<T: serde::Serialize>(req_id: u16, record: &T) {
    let mut buf = [0u8; 192];
    match postcard::to_slice(record, &mut buf) {
        Ok(bytes) => {
            let n = bytes.len();
            console::mgmt_chunk(req_id, mgmt::MGMT_OK, 0, true, &buf[..n]).await;
        }
        Err(_) => respond_empty(req_id, mgmt::MGMT_STORAGE).await,
    }
}

// ---- US 915 hop schedule ----------------------------------------------------------------------

/// US 915: follow gateway `gw_addr`'s hop schedule (FCC §15.247). Its beacons are sealed under its
/// address-derived network key, which also seeds the schedule; the first send scans for it.
async fn start_hopping(net: &mut Net<NODE_PEERS>, gw_addr: u32) {
    let cfg = FhssConfig {
        network_key: Some(gateway_network_key(gw_addr)),
        ..FhssConfig::default()
    };
    if let Err(e) = net.enable_fhss(FhssRole::Node, cfg).await {
        warn!(target: "node", "hopping not started ({e})");
    }
}

/// A confirmed send to the gateway, `reps` tries: on a hop slot whose beacon the node just heard
/// (US 915), else the duty-mode send.
async fn radio_send(net: &mut Net<NODE_PEERS>, dest: u32, data: &[u8], reps: u8) -> SendResult {
    if net.access() != NetAccess::Fhss {
        return net.send(dest, data, true, reps).await;
    }
    let (mut tries, mut scans, mut missed) = (0u8, 0u16, 0u8);
    loop {
        let slot = net.fhss_node_tick().await;
        match slot.state {
            FhssState::Scanning => {
                scans += 1;
                if scans >= FHSS_SCAN_TICKS || console::mgmt_pending() {
                    return SendResult::NotSynced;
                }
                continue;
            }
            FhssState::Synced if !slot.got_beacon => {
                missed += 1;
                if missed >= FHSS_MISSED_BEACONS {
                    return SendResult::NotDelivered;
                }
                continue;
            }
            FhssState::Synced => {}
        }
        match net.fhss_send(dest, data, true).await {
            SendResult::Delivered => return SendResult::Delivered,
            SendResult::DutyLimited => {}
            SendResult::NotDelivered => {
                tries += 1;
                if tries >= reps.max(1) {
                    return SendResult::NotDelivered;
                }
                net.fhss_retry_backoff().await;
            }
            other => return other,
        }
    }
}

/// Re-anchor to the hop schedule with nothing to send; `false` if no beacon was heard.
async fn fhss_resync(net: &mut Net<NODE_PEERS>) -> bool {
    let _ = net.wake().await;
    let mut heard = false;
    for _ in 0..4 {
        let slot = net.fhss_node_tick().await;
        if slot.state == FhssState::Synced && slot.got_beacon {
            heard = true;
            break;
        }
        if slot.state == FhssState::Scanning {
            break;
        }
    }
    let _ = net.sleep().await;
    heard
}

// ---- the remote shell -------------------------------------------------------------------------

/// The last remote command run, for at-least-once dedup: bit 24 = valid, bits 23..16 = result,
/// bits 15..0 = cmd_id. RAM only (the delivery retries that matter are within a boot).
static LAST_REMOTE_CMD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// The host-process `epoch` of the last remote command; the dedup keys on `(epoch, cmd_id)`.
static LAST_REMOTE_EPOCH: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// 0 if the last command's reply all went up, else its first undelivered chunk + 1.
static LAST_REMOTE_LOST: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Run one remote-shell line (de-duplicated against the gateway's re-deliveries) and stream its
/// reply.
async fn remote_shell(net: &mut Net<NODE_PEERS>, gw: u32, epoch: u32, cmd_id: u16, line: &str) {
    use core::sync::atomic::Ordering::Relaxed;
    let mut sink = RadioSink::new(net, gw, cmd_id);
    let last = LAST_REMOTE_CMD.load(Relaxed);
    if last & (1 << 24) != 0 && (last & 0xFFFF) as u16 == cmd_id && LAST_REMOTE_EPOCH.load(Relaxed) == epoch {
        let result = ((last >> 16) & 0xFF) as u8;
        let lost = LAST_REMOTE_LOST.load(Relaxed);
        if lost == 0 {
            info!(target: "node", "remote shell: command {} repeated, not run again", cmd_id);
            sink.done(result).await;
            return;
        }
        // The first run's reply never all went up: number the repeat past the lost chunk (and the
        // one after it), so the host sees a gap and reports the reply truncated.
        warn!(target: "node", "remote shell: command {} repeated, its reply was lost", cmd_id);
        sink.chunk = (lost as u16).wrapping_add(1);
        let _ = sink
            .push("(reply lost on the air: ran once, not run again)\r\n")
            .await;
        sink.done(result).await;
        return;
    }
    info!(target: "node", "remote shell: {}", line);
    let (result, reboot) = shell::stream_line(line, &mut sink).await;
    LAST_REMOTE_EPOCH.store(epoch, Relaxed);
    LAST_REMOTE_LOST.store(sink.lost.map_or(0, |c| u32::from(c) + 1), Relaxed);
    LAST_REMOTE_CMD.store((1 << 24) | ((result as u32) << 16) | cmd_id as u32, Relaxed);
    if reboot {
        cortex_m::peripheral::SCB::sys_reset();
    }
}

/// Streams a remote-shell reply as `NodeShellChunk` uplinks (≤ [`RADIO_SHELL_CHUNK`] bytes each,
/// confirmed), so a reply of any length goes through one chunk-sized buffer.
struct RadioSink<'a> {
    net: &'a mut Net<NODE_PEERS>,
    gw: u32,
    cmd_id: u16,
    chunk: u16,
    buf: heapless::String<RADIO_SHELL_CHUNK>,
    ok: bool,
    lost: Option<u16>,
}

impl<'a> RadioSink<'a> {
    fn new(net: &'a mut Net<NODE_PEERS>, gw: u32, cmd_id: u16) -> Self {
        Self {
            net,
            gw,
            cmd_id,
            chunk: 0,
            buf: heapless::String::new(),
            ok: true,
            lost: None,
        }
    }

    async fn flush(&mut self, last: bool, result: u8) -> bool {
        let msg = NodeMsg::Shell(NodeShellChunk {
            cmd_id: self.cmd_id,
            result,
            chunk: self.chunk,
            last,
            text: self.buf.as_str(),
        });
        let mut wire = [0u8; MAX_RADIO_PAYLOAD];
        let Ok(n) = encode_node_msg(&msg, &mut wire) else {
            self.ok = false;
            self.lost.get_or_insert(self.chunk);
            return false;
        };
        let delivered = matches!(
            radio_send(self.net, self.gw, &wire[..n], CHUNK_REPS).await,
            SendResult::Delivered
        );
        self.buf.clear();
        self.chunk = self.chunk.wrapping_add(1);
        if !delivered {
            warn!(target: "node", "remote shell: response chunk {} not delivered, rest dropped", self.chunk - 1);
            self.ok = false;
            self.lost.get_or_insert(self.chunk - 1);
        }
        delivered
    }
}

impl ShellSink for RadioSink<'_> {
    async fn push(&mut self, text: &str) -> bool {
        if !self.ok {
            return false;
        }
        let mut rest = text;
        while !rest.is_empty() {
            let space = RADIO_SHELL_CHUNK - self.buf.len();
            let mut take = rest.len().min(space);
            while take > 0 && !rest.is_char_boundary(take) {
                take -= 1;
            }
            if take == 0 {
                if !self.flush(false, 0).await {
                    return false;
                }
                continue;
            }
            let Some((head, tail)) = rest.split_at_checked(take) else {
                break;
            };
            let _ = self.buf.push_str(head);
            rest = tail;
            if self.buf.len() == RADIO_SHELL_CHUNK && !self.flush(false, 0).await {
                return false;
            }
        }
        true
    }

    async fn done(&mut self, result: u8) -> bool {
        if !self.ok {
            return false;
        }
        self.flush(true, result).await
    }
}
