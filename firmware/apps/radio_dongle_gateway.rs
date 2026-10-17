//! radio_dongle_gateway (TOWER IoT Kit product firmware): the network coordinator.
//!
//! The USB Radio Dongle bridges its secured radio network to the host over the framed
//! console, as a **transparent bridge**: every authenticated uplink is forwarded
//! verbatim (`Uplink` frames; the host decodes the radio-application payload), so new
//! node app types never require a gateway firmware change. What the gateway *does* own
//! is coordination:
//!
//! * the **node registry**: paired ids + AES keys + names, persisted in `NS_APP`
//!   EEPROM buckets (`tower-gateway::registry`), accessed bucket-at-a-time (never
//!   RAM-resident; see the RAM budget note below) and mirrored into the net layer's
//!   peer table at boot;
//! * **pairing**: an OTA window opened on host request (`MgmtOp::PairingOpen`; the link
//!   key comes from an X25519 key agreement with the joining node, so the request's `key`
//!   field is unused) and cable-paired installs (`MgmtOp::NodeAdd`, host-minted key);
//! * the **downlink queue**: opaque host-built payloads (`tower-gateway::queue`,
//!   RAM-only; a reboot drops it and the host re-queues on the `Hello` session bump)
//!   delivered on a sleeping node's next uplink, advertised via the ACK pending flag;
//! * **radio diagnostics**: ambient channel RSSI at a settable cadence plus per-TX
//!   outcome reports (`RadioStat` frames) feeding the host's running channel graph.
//!
//! Single-owner design: one main loop owns `Net` + queue (no `&mut Net` sharing across
//! tasks) and multiplexes recv slices, management requests, the pairing window, and
//! the stats tick.
//!
//! **RAM budget** (20 KB part, ~9 KB stack region; bench-measured gateway high-water 7360 B
//! (2026-07-12, `/system stack print`); see the stack-overflow history): everything this app keeps resident beyond the SDK
//! baseline must fit in ~2 KB, or the stack loses the difference. Hence: registry on
//! EEPROM (transient ~270 B bucket locals on the stack), a 4-item downlink queue,
//! and 12-byte packed link stats. Check `just size app radio_dongle_gateway` after
//! growing anything here.
//!
//!   just build app radio_dongle_gateway
//!   just run   app radio_dongle_gateway

#![no_std]
#![no_main]

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use tower::led::{self, LedChannel, Pattern, Polarity, Step};
use tower::radio::Spirit1;
use tower::radio::config::Band;
use tower::radio::net::{Net, NetConfig, PairingHost, SendResult, peer_addr_valid};
use tower::storage::{NS_APP, NS_SHELL, Nv, Scoped};
use tower::{app, board::Board, console, shell, watchdog};
use tower_gateway::queue::{PushError, Queue};
use tower_gateway::registry::{self, BucketIo, IoError, NodeRecord, RegistryError};
use tower_protocol::mgmt::{self, DeviceInfo, DeviceRole, MgmtOp, NodeEntry, NodeKey, Paired, QueueId};
use tower_protocol::msg::{MgmtRequest, RadioStat};
use tower_protocol::{MsgType, decode_frame};

// --- persistence (NS_APP) --------------------------------------------------------

/// Registry format marker (`tower_gateway::registry::FORMAT`).
const KEY_FORMAT: u8 = 0x00;
/// Registry buckets, locals `0x10..=0x12` (one `tower-gateway` bucket per KV value;
/// `registry::BUCKETS` = 3). `0x13..=0x15` stay reserved for bucket growth. The gateway's
/// own radio address is the `addr` base setting (`shell::radio_addr`).
const KEY_BUCKET_BASE: u8 = 0x10;

/// The registry's bucket store: `NS_APP` locals `0x10..=0x15`.
#[derive(Clone, Copy)]
struct EepromBuckets {
    kv: Scoped,
}

impl BucketIo for EepromBuckets {
    fn load(&self, index: usize, out: &mut [u8]) -> Result<Option<usize>, IoError> {
        self.kv
            .get_bytes(KEY_BUCKET_BASE + index as u8, out)
            .map_err(|_| IoError)
    }
    fn store(&mut self, index: usize, bytes: &[u8]) -> Result<(), IoError> {
        self.kv
            .set_bytes(KEY_BUCKET_BASE + index as u8, bytes)
            .map_err(|_| IoError)
    }
}

// --- settings (NS_SHELL locals; base `identity` owns 0x00) ------------------------

const SET_STATS_PERIOD: u8 = 0x10;
const SET_BAND: u8 = 0x11;
const SET_CHANNEL: u8 = 0x12;
const SET_KEYED_JOIN: u8 = 0x13;

static GW_SETTINGS: &[shell::Setting] = &[
    shell::Setting {
        key: SET_STATS_PERIOD,
        name: "stats-period",
        kind: shell::Kind::Uint { min: 0, max: 60_000 },
        default: "500",
    },
    shell::Setting {
        key: SET_BAND,
        name: "band",
        kind: shell::Kind::Enum(&["eu868", "us915"]),
        default: "eu868",
    },
    shell::Setting {
        key: SET_CHANNEL,
        name: "channel",
        kind: shell::Kind::Uint { min: 0, max: 2 },
        default: "0",
    },
    // Answer keyed joins (E4) from registered nodes. On by default: the exchange is mutually
    // authenticated under the node's own key and puts nothing secret on air. Off = the
    // operator's switch for a deployment that admits nodes only through its own procedure;
    // refusals are logged (`net` target, rate-limited). Re-read every `POLICY_REFRESH`.
    shell::Setting {
        key: SET_KEYED_JOIN,
        name: "keyed-join",
        kind: shell::Kind::Bool,
        default: "on",
    },
];

static GW_COMMANDS: &[shell::Entry] = &[shell::Entry::menu(
    "gateway",
    &[shell::Entry::cmd("status", shell::Args::None, cmd_status)],
)];

// Live counters for `/gateway status` (shell handlers are plain fns with no app-state
// access; M0+ atomics are load/store only, which is all these need).
static STAT_ID: AtomicU32 = AtomicU32::new(0);
static STAT_NODES: AtomicU32 = AtomicU32::new(0);
static STAT_UPLINKS: AtomicU32 = AtomicU32::new(0);
static STAT_QUEUED: AtomicU32 = AtomicU32::new(0);

fn cmd_status(ctx: &mut shell::Ctx<'_>, _args: &[&str]) -> shell::Outcome {
    let _ = write!(
        ctx,
        "gateway: {:08X}\r\nnodes: {}\r\nuplinks: {}\r\nqueued: {}\r\n",
        STAT_ID.load(Ordering::Relaxed),
        STAT_NODES.load(Ordering::Relaxed),
        STAT_UPLINKS.load(Ordering::Relaxed),
        STAT_QUEUED.load(Ordering::Relaxed),
    );
    shell::Outcome::ok()
}

// --- timing ------------------------------------------------------------------------

/// One receive slice of the main loop: short enough that management requests and the
/// stats tick stay responsive, long enough that the loop isn't spinning.
const RECV_SLICE: Duration = Duration::from_millis(250);
/// One pairing-window slice (management latency ceiling while pairing).
const PAIR_SLICE: Duration = Duration::from_secs(2);
/// Node TX→RX turnaround before a downlink delivery (mirror of the ACK turnaround: the
/// node checks the pending flag on our ACK, then switches to RX).
const DOWNLINK_TURNAROUND: Duration = Duration::from_millis(20);
/// Downlink delivery repetitions. Deliberately low (one retry): the node listens for
/// ~1 s right after its uplink, and a longer retransmit tail would hold the
/// single-owner loop out of `recv` while a mid-response node is uplinking its next
/// shell chunk.
const DOWNLINK_REPS: u8 = 2;
/// Default downlink TTL when the host passes `ttl == 0`.
const DEFAULT_TTL_S: u16 = 3600;
/// How often the main loop re-reads the `keyed-join` setting, so `/system settings set
/// keyed-join=off` takes effect without a reboot. Not every iteration: a KV lookup scans (and
/// CRC-checks) the active EEPROM half, a few ms of CPU on this part.
const POLICY_REFRESH: Duration = Duration::from_secs(2);

static CH: LedChannel = LedChannel::new();
/// Slow "gateway alive" heartbeat on the on-board LED.
static HEARTBEAT: Pattern = &[Step::on(30), Step::off(1970)];
/// Fast blink while the OTA pairing window is open.
static PAIRING: Pattern = &[Step::on(100), Step::off(150)];

/// The net layer's `NetConfig::key` for the gateway. **Not a secret, and it must never
/// authenticate anything:** it is derived from the gateway's own address, which rides in
/// clear in every frame, with a public constant, so anyone can compute it. (An earlier note
/// here claimed unregistered traffic "must simply fail CCM auth, which any value achieves". That was
/// wrong: an attacker sealed DATA frames from unregistered addresses under this key and the
/// gateway ACKed them and forwarded them to the host as genuine uplinks.) The gateway therefore
/// runs **registered peers only** (`set_default_lane(false)` right after `Net::new`): no
/// default lane, so this key is never used to open a frame. It only exists because `NetConfig`
/// requires one; no legitimate node uses it (each has its own key; pairing derives one by X25519).
fn default_lane_key(addr: u32) -> [u8; 16] {
    let mut k = [0u8; 16];
    for (i, chunk) in k.chunks_mut(4).enumerate() {
        chunk.copy_from_slice(&(addr ^ (0x9E37_79B9u32.rotate_left(i as u32 * 8))).to_le_bytes());
    }
    k
}

/// RAM-only per-node link stats. Packed to 12 B/entry (16 entries = 192 B of the app
/// future); deliberately NOT persisted; per-uplink EEPROM writes would burn the part
/// (docs/storage.md). `NodeEntry.last_seen` is documented as "since gateway boot".
#[derive(Clone, Copy)]
struct LinkStat {
    addr: u32,
    /// `Instant::as_secs()` truncated; wraps after 136 years of uptime.
    seen_s: u32,
    rssi: i8,
    /// Saturating (reported as-is in the u32 wire field).
    uplinks: u16,
}

// The registry, the net peer table, and the link-stat array MUST stay the same size:
// if the registry could hold more nodes than the peer table, the boot mirror would
// silently drop the excess and those nodes' uplinks would fail CCM auth against the
// default lane: paired nodes going dead with no host-visible signal. MAX_PEERS has
// moved twice already (64→32→16); pin the coupling here, the one crate that sees both.
const _: () = assert!(registry::CAPACITY == tower::radio::net::MAX_PEERS);

// One link-stat slot per registered node (registry CAPACITY == net-layer MAX_PEERS,
// pinned above): a slot fills on a node's first uplink and frees on NodeRemove.
struct Stats([Option<LinkStat>; registry::CAPACITY]);

impl Stats {
    const fn new() -> Self {
        Self([None; registry::CAPACITY])
    }
    fn on_uplink(&mut self, addr: u32, rssi: i16) {
        let rssi = rssi.clamp(i8::MIN as i16, i8::MAX as i16) as i8;
        let now = Instant::now().as_secs() as u32;
        if let Some(s) = self.0.iter_mut().flatten().find(|s| s.addr == addr) {
            s.seen_s = now;
            s.rssi = rssi;
            s.uplinks = s.uplinks.saturating_add(1);
            return;
        }
        if let Some(slot) = self.0.iter_mut().find(|s| s.is_none()) {
            *slot = Some(LinkStat {
                addr,
                seen_s: now,
                rssi,
                uplinks: 1,
            });
        }
    }
    fn get(&self, addr: u32) -> Option<&LinkStat> {
        self.0.iter().flatten().find(|s| s.addr == addr)
    }
    fn remove(&mut self, addr: u32) {
        for s in self.0.iter_mut() {
            if matches!(s, Some(st) if st.addr == addr) {
                *s = None;
            }
        }
    }
}

/// An armed OTA pairing window (delayed `MgmtResponse`: resolves on join/expiry/cancel).
struct PairingSession {
    req_id: u16,
    /// The window's precomputed X25519 ephemeral (zeroized when the session drops).
    host: PairingHost,
    deadline: Instant,
}

/// Chunked management-response sender: accumulates record bytes into ≤192-byte `data`
/// chunks, flushing as they fill (the app-driven-chunking contract of
/// [`console::mgmt_chunk`]). Record boundaries need not align with chunk boundaries;
/// the host concatenates all `data` before decoding the record stream.
struct ChunkStream {
    req_id: u16,
    chunk: u16,
    len: usize,
    // 128, not the full 192-byte chunk budget: this buffer lives across awaits (in
    // the app future); a few more chunks on a 16-node list is cheaper than the RAM.
    buf: [u8; 128],
}

impl ChunkStream {
    fn new(req_id: u16) -> Self {
        Self {
            req_id,
            chunk: 0,
            len: 0,
            buf: [0; 128],
        }
    }
    async fn push(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.len == self.buf.len() {
                console::mgmt_chunk(
                    self.req_id,
                    mgmt::MGMT_OK,
                    self.chunk,
                    false,
                    &self.buf[..self.len],
                )
                .await;
                self.chunk = self.chunk.wrapping_add(1);
                self.len = 0;
            }
            let take = bytes.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
            self.len += take;
            bytes = &bytes[take..];
        }
    }
    async fn finish(self) {
        console::mgmt_chunk(
            self.req_id,
            mgmt::MGMT_OK,
            self.chunk,
            true,
            &self.buf[..self.len],
        )
        .await;
    }
}

/// Reply with a result code and no records.
async fn respond_empty(req_id: u16, result: u8) {
    console::mgmt_chunk(req_id, result, 0, true, &[]).await;
}

/// Reply `MGMT_OK` with a single postcard record (largest single record is a
/// max-name `DeviceInfo` with its `firmware_version`, ~57 B; 96 covers every one-record reply).
async fn respond_record<T: serde::Serialize>(req_id: u16, record: &T) {
    let mut buf = [0u8; 96];
    match postcard::to_slice(record, &mut buf) {
        Ok(bytes) => {
            let n = bytes.len();
            console::mgmt_chunk(req_id, mgmt::MGMT_OK, 0, true, &buf[..n]).await;
        }
        Err(_) => respond_empty(req_id, mgmt::MGMT_STORAGE).await,
    }
}

fn read_u32_setting(kv: Nv, local: u8, default: u32) -> u32 {
    let mut b = [0u8; 4];
    match kv.scope(NS_SHELL).get_bytes(local, &mut b) {
        Ok(Some(4)) => u32::from_le_bytes(b),
        _ => default,
    }
}

fn read_bool_setting(kv: Nv, local: u8, default: bool) -> bool {
    let mut b = [0u8; 1];
    match kv.scope(NS_SHELL).get_bytes(local, &mut b) {
        Ok(Some(1)) => b[0] != 0,
        _ => default,
    }
}

fn read_band_setting(kv: Nv) -> Band {
    let mut b = [0u8; 8];
    match kv.scope(NS_SHELL).get_bytes(SET_BAND, &mut b) {
        Ok(Some(n)) if &b[..n] == b"us915" => Band::Us915,
        _ => Band::Eu868,
    }
}

async fn run(b: Board) {
    // Hardware safety net: a hung gateway takes the whole network down; reset it within
    // ~26 s (the L0 hardware ceiling) rather than waiting for someone to notice. Liveness-
    // gated on the main loop below: the feeder task alone only proves the executor runs,
    // and the 2026-07-13 wedge (main loop parked on console backpressure after a lost
    // TC flag; LED blinking, IWDG fed, gateway deaf) sailed right past an ungated feeder.
    // 10 s comfortably exceeds the longest legitimate iteration (a 2 s pairing slice, a
    // full downlink retry tail); a loop older than that is gone, not busy.
    watchdog::enable(b.iwdg, b.spawner, Duration::from_secs(26));
    watchdog::require_checkin(Duration::from_secs(10));

    let led = led::init(
        b.spawner,
        Output::new(b.led, Level::Low, Speed::Low),
        &CH,
        Polarity::ActiveHigh,
    );
    led.set_background(Some(HEARTBEAT));

    let kv = b.kv;
    let app_kv = kv.scope(NS_APP);
    let mut buckets = EepromBuckets { kv: app_kv };

    // The gateway's own radio address = the `addr` base setting (pinned or
    // UID-derived); set it with `system settings set addr` (e.g. `addr=random`).
    let addr = shell::radio_addr(kv);
    STAT_ID.store(addr, Ordering::Relaxed);
    // Registry format guard: buckets stamped with another marker were written with another
    // record layout (firmware from another checkout). Refuse them instead of mis-decoding them
    // as empty and stamping the marker back, which would silently discard every pairing. An
    // unset marker (a fresh store) is stamped. Nothing is migrated.
    let mut fmt = [0u8; 1];
    let foreign_format =
        matches!(app_kv.get_bytes(KEY_FORMAT, &mut fmt), Ok(Some(1)) if fmt[0] != registry::FORMAT);
    if foreign_format {
        log::error!(
            target: "gateway",
            "registry format {} is not this firmware's ({}), registry left untouched; reflash from the same checkout",
            fmt[0],
            registry::FORMAT
        );
    } else {
        let _ = app_kv.set_bytes(KEY_FORMAT, &[registry::FORMAT]);
    }

    let band = read_band_setting(kv);
    let channel = read_u32_setting(kv, SET_CHANNEL, 0) as u8;
    let mut stats_period_ms = read_u32_setting(kv, SET_STATS_PERIOD, 500);

    let radio = Spirit1::new(
        b.radio_spi,
        b.radio_sck,
        b.radio_mosi,
        b.radio_miso,
        b.radio_cs,
        b.radio_sdn,
        b.radio_irq,
    );
    let mut net = match Net::new(
        radio,
        kv,
        NetConfig {
            addr,
            key: default_lane_key(addr),
            band,
            channel,
        },
    )
    .await
    {
        Ok(n) => n,
        Err(e) => {
            log::error!(target: "gateway", "net init: {e}; gateway down");
            return;
        }
    };
    // Registered peers only: a frame from any address not in the peer table (never paired, or
    // removed) is dropped on its clear header: no CCM, no ACK, no uplink to the host, no
    // default-lane EEPROM writes. See `default_lane_key` for why this is not optional.
    net.set_default_lane(false);
    net.set_keyed_join(read_bool_setting(kv, SET_KEYED_JOIN, true));

    // Install every registered node into the peer table, bucket-at-a-time. CAPACITY ==
    // MAX_PEERS is pinned above, so a refusal here is a real invariant break: a node
    // the registry holds but the radio won't decrypt for. Say so instead of going mute.
    // (Skipped under the format guard: those buckets must not be interpreted.)
    let mut node_count = 0u32;
    if !foreign_format {
        for i in 0..registry::BUCKETS {
            for rec in registry::load(&buckets, i).iter() {
                if !net.add_peer(rec.addr, &rec.key) {
                    warn!(target: "gateway", "peer table refused {:08X}, node will not decrypt", rec.addr);
                }
                node_count += 1;
            }
        }
    }
    STAT_NODES.store(node_count, Ordering::Relaxed);

    info!(
        target: "gateway",
        "GATEWAY {:08X}: {} node(s), band {}, ch {}",
        addr,
        node_count,
        match band { Band::Eu868 => "eu868", Band::Us915 => "us915" },
        channel
    );

    let mut queue = Queue::new();
    let mut stats = Stats::new();
    let mut pairing: Option<PairingSession> = None;
    let mut last_stat = Instant::now();
    let mut last_policy = Instant::now();

    loop {
        // 0. Liveness check-in (see the watchdog arming above): every await in this loop
        //    is bounded, so a stale check-in means parked-forever, and the IWDG resets us.
        watchdog::checkin();
        if last_policy.elapsed() >= POLICY_REFRESH {
            last_policy = Instant::now();
            net.set_keyed_join(read_bool_setting(kv, SET_KEYED_JOIN, true));
        }

        // 1. Serve management requests (host → gateway), but at most two per loop
        //    iteration, so a host polling faster than the response-transmit time can't
        //    starve the radio recv slice below (depth-1 channel: a dropped burst is
        //    just retried by the host anyway).
        for _ in 0..2 {
            let Some(frame) = console::mgmt_try_next() else {
                break;
            };
            let Ok((MsgType::MgmtRequest, _seq, payload)) = decode_frame(&frame) else {
                continue;
            };
            let Ok(req) = postcard::from_bytes::<MgmtRequest>(payload) else {
                continue;
            };
            handle_mgmt(
                req,
                &mut net,
                &mut buckets,
                &mut queue,
                &mut stats,
                &mut pairing,
                &mut stats_period_ms,
                addr,
                band,
                channel,
                &led,
            )
            .await;
        }

        let now_ms = Instant::now().as_millis();

        // 2. Radio: either a pairing-window slice or a receive slice; the two share
        //    the one radio, and pairing mode is explicit + short-lived.
        if let Some(sess) = pairing.as_mut() {
            if registry::count(&buckets) >= registry::CAPACITY {
                // The registry filled while the window was open (a concurrent cable
                // NodeAdd): close early instead of letting a node complete the JOIN,
                // persist its side, and then be refused, half-paired and unreachable.
                respond_empty(sess.req_id, mgmt::MGMT_FULL).await;
                led.set_background(Some(HEARTBEAT));
                pairing = None;
            } else if Instant::now() >= sess.deadline {
                respond_empty(sess.req_id, mgmt::MGMT_TIMEOUT).await;
                led.set_background(Some(HEARTBEAT));
                pairing = None;
            } else {
                let slice = PAIR_SLICE.min(sess.deadline.saturating_duration_since(Instant::now()));
                // `open_pairing` never answers a JOIN_REQ for an already-registered (or reserved,
                // or our own) address; OTA pairing only adds NEW nodes (docs/gateway.md). One
                // call can outlast the slice by a handshake (the nonce wait, ~1.2 s compute, the
                // 1.8 s confirm wait: ~3.4 s) plus ~1.2 s for the next ephemeral at its start:
                // ~7 s worst case, still inside the 10 s liveness check-in.
                if let Some((node_addr, key)) = net.open_pairing(&mut sess.host, slice).await {
                    let req_id = sess.req_id;
                    led.set_background(Some(HEARTBEAT));
                    pairing = None;
                    commit_pairing(node_addr, key, req_id, &mut net, &mut buckets).await;
                }
            }
        } else if let Some(rx) = net.recv(RECV_SLICE).await {
            // 3. One authenticated uplink: account, forward verbatim, then deliver a
            //    queued downlink into the node's post-uplink RX window (if any).
            stats.on_uplink(rx.src, rx.rssi);
            STAT_UPLINKS.store(
                STAT_UPLINKS.load(Ordering::Relaxed).wrapping_add(1),
                Ordering::Relaxed,
            );
            console::uplink(rx.src, rx.counter, rx.rssi, rx.lqi, rx.data()).await;

            if let Some(item) = queue.peek_for(rx.src, now_ms) {
                let (item_id, mut data, len) = (item.id, [0u8; 74], item.data().len());
                data[..len].copy_from_slice(item.data());
                Timer::after(DOWNLINK_TURNAROUND).await;
                let result = net.send(rx.src, &data[..len], true, DOWNLINK_REPS).await;
                let outcome = match result {
                    SendResult::Delivered => {
                        queue.pop(item_id);
                        mgmt::TX_DELIVERED
                    }
                    SendResult::NotDelivered => mgmt::TX_NOT_DELIVERED,
                    SendResult::Busy => mgmt::TX_BUSY,
                    SendResult::DutyLimited => mgmt::TX_DUTY_LIMITED,
                    _ => mgmt::TX_ERROR,
                };
                console::radio_stat(RadioStat::Tx {
                    dest: rx.src,
                    item: item_id,
                    outcome,
                    ack_rssi: net.last_ack().map(|m| m.rssi),
                })
                .await;
            }
            net.set_pending(rx.src, queue.count_for(rx.src) > 0);
            STAT_QUEUED.store(queue.len() as u32, Ordering::Relaxed);
        }
        // A registered node's keyed join (E4) was answered inside recv(): nothing to
        // install (its key and lane already are), just say so.
        if let Some(node_addr) = net.take_keyed_join() {
            info!(target: "gateway", "node {:08X} joined (keyed)", node_addr);
        }

        // 4. Housekeeping: TTL expiry (reported as TX_EXPIRED) + the ambient-RSSI tick.
        let mut expired: heapless::Vec<(u16, u32), { tower_gateway::queue::QUEUE_CAP }> =
            heapless::Vec::new();
        queue.expire(now_ms, |id, node| {
            let _ = expired.push((id, node));
        });
        for &(id, node) in expired.iter() {
            console::radio_stat(RadioStat::Tx {
                dest: node,
                item: id,
                outcome: mgmt::TX_EXPIRED,
                ack_rssi: None,
            })
            .await;
            net.set_pending(node, queue.count_for(node) > 0);
        }
        if !expired.is_empty() {
            STAT_QUEUED.store(queue.len() as u32, Ordering::Relaxed);
        }

        // Ambient channel RSSI: the sample is a free by-product of the recv slices: each idle
        // RX timeout latches the channel level as it SABORTs (`Net::take_idle_rssi`). The old
        // `rssi_sample()` here opened its own ~20 ms RX window (flushing the FIFO) and then left
        // the radio READY across the console write: every 500 ms the gateway went deaf long
        // enough to cost ~8-14 % of first-attempt uplinks. No fresh idle sample yet (the slices
        // kept catching frames) → nothing to report; retry next iteration.
        if stats_period_ms > 0
            && last_stat.elapsed() >= Duration::from_millis(stats_period_ms as u64)
            && let Some(rssi) = net.take_idle_rssi()
        {
            last_stat = Instant::now();
            console::radio_stat(RadioStat::Channel { channel, rssi }).await;
        }
    }
}

/// Commit a joined node: persist it (UNNAMED: the host auto-names from the first
/// `NodeInfo` uplink via `NodeUpdate`, keeping this firmware payload-agnostic), install
/// the peer, and resolve the delayed `PairingOpen` response. Insert-only
/// (`registry::add_new`): an OTA join must never overwrite a registered node's key (the net
/// layer already refuses such a JOIN_REQ; this is the registry-side backstop).
async fn commit_pairing(
    node_addr: u32,
    key: [u8; 16],
    req_id: u16,
    net: &mut Net,
    buckets: &mut EepromBuckets,
) {
    let rec = NodeRecord {
        addr: node_addr,
        key,
        flags: mgmt::NODE_FLAG_UNNAMED,
        name: heapless::String::new(),
    };
    match registry::add_new(buckets, &rec) {
        Ok(()) => {
            if !net.add_peer(node_addr, &key) {
                warn!(target: "gateway", "peer table refused {:08X}, node will not decrypt", node_addr);
            }
            STAT_NODES.store(registry::count(buckets) as u32, Ordering::Relaxed);
            info!(target: "gateway", "paired node {:08X}", node_addr);
            respond_record(req_id, &Paired { addr: node_addr }).await;
        }
        Err(RegistryError::Exists) => {
            warn!(target: "gateway", "pairing: {:08X} is already registered; remove it first to re-pair", node_addr);
            respond_empty(req_id, mgmt::MGMT_BAD_ARG).await;
        }
        Err(RegistryError::Full) => respond_empty(req_id, mgmt::MGMT_FULL).await,
        Err(_) => respond_empty(req_id, mgmt::MGMT_STORAGE).await,
    }
}

#[allow(clippy::too_many_arguments)] // the single-owner loop hands its whole state down once
async fn handle_mgmt(
    req: MgmtRequest<'_>,
    net: &mut Net,
    buckets: &mut EepromBuckets,
    queue: &mut Queue,
    stats: &mut Stats,
    pairing: &mut Option<PairingSession>,
    stats_period_ms: &mut u32,
    addr: u32,
    band: Band,
    channel: u8,
    led: &led::Led,
) {
    let req_id = req.req_id;
    let now_ms = Instant::now().as_millis();
    let gw_addr = addr; // the arms below destructure their own `addr`
    match req.op {
        MgmtOp::Describe => {
            let name = console::firmware_name();
            respond_record(
                req_id,
                &DeviceInfo {
                    role: DeviceRole::Gateway,
                    addr,
                    band: match band {
                        Band::Eu868 => mgmt::BAND_EU868,
                        Band::Us915 => mgmt::BAND_US915,
                    },
                    channel,
                    node_capacity: registry::CAPACITY as u8,
                    node_count: registry::count(buckets) as u8,
                    provisioned: true,
                    gw_addr: addr,
                    firmware_name: name.as_str(),
                    // The same version the boot `Hello` carries, so a host that attached after
                    // boot (missed the Hello) still shows it.
                    firmware_version: console::firmware_version(),
                },
            )
            .await;
        }
        MgmtOp::NodeList => {
            let now_s = Instant::now().as_secs() as u32;
            let mut stream = ChunkStream::new(req_id);
            for i in 0..registry::BUCKETS {
                // Encode one bucket's entries into a staging buffer synchronously, so
                // the ~270 B bucket local lives on the stack, not across the await.
                let mut staged: heapless::Vec<u8, 256> = heapless::Vec::new();
                for rec in registry::load(buckets, i).iter() {
                    let st = stats.get(rec.addr);
                    let entry = NodeEntry {
                        addr: rec.addr,
                        name: rec.name.as_str(),
                        flags: rec.flags,
                        last_seen: st
                            .map(|s| now_s.saturating_sub(s.seen_s))
                            .unwrap_or(mgmt::LAST_SEEN_NEVER),
                        rssi: st.map(|s| s.rssi).unwrap_or(mgmt::RSSI_NONE),
                        uplinks: st.map(|s| s.uplinks as u32).unwrap_or(0),
                        queued: queue.count_for(rec.addr) as u8,
                    };
                    let mut scratch = [0u8; 64];
                    if let Ok(bytes) = postcard::to_slice(&entry, &mut scratch) {
                        let _ = staged.extend_from_slice(bytes); // 6 × ≤42 B fits 256
                    }
                }
                stream.push(&staged).await;
            }
            stream.finish().await;
        }
        MgmtOp::NodeAdd {
            addr,
            key,
            name,
            flags,
        } => {
            // Same address rules as OTA pairing and `Net::add_peer`: not reserved (0 /
            // broadcast) and not the gateway's own address (a node sharing it would collide CCM
            // nonces under its key). A cable NodeAdd MAY overwrite an existing node; it is an
            // authenticated host action (re-provision), unlike an OTA join.
            if !peer_addr_valid(addr, gw_addr) || name.len() > mgmt::MAX_NODE_NAME {
                respond_empty(req_id, mgmt::MGMT_BAD_ARG).await;
                return;
            }
            let mut n: heapless::String<{ registry::MAX_NAME }> = heapless::String::new();
            let _ = n.push_str(name); // fits: checked above
            match registry::add(
                buckets,
                &NodeRecord {
                    addr,
                    key,
                    flags,
                    name: n,
                },
            ) {
                Ok(()) => {
                    if !net.add_peer(addr, &key) {
                        warn!(target: "gateway", "peer table refused {:08X}, node will not decrypt", addr);
                    }
                    STAT_NODES.store(registry::count(buckets) as u32, Ordering::Relaxed);
                    info!(target: "gateway", "node {:08X} added ({})", addr, name);
                    respond_empty(req_id, mgmt::MGMT_OK).await;
                }
                Err(RegistryError::Full) => respond_empty(req_id, mgmt::MGMT_FULL).await,
                Err(_) => respond_empty(req_id, mgmt::MGMT_STORAGE).await,
            }
        }
        MgmtOp::NodeRemove { addr } => match registry::remove(buckets, addr) {
            Ok(()) => {
                net.remove_peer(addr);
                queue.drop_node(addr);
                stats.remove(addr);
                STAT_NODES.store(registry::count(buckets) as u32, Ordering::Relaxed);
                STAT_QUEUED.store(queue.len() as u32, Ordering::Relaxed);
                info!(target: "gateway", "node {:08X} removed", addr);
                respond_empty(req_id, mgmt::MGMT_OK).await;
            }
            Err(RegistryError::NotFound) => respond_empty(req_id, mgmt::MGMT_NOT_FOUND).await,
            Err(_) => respond_empty(req_id, mgmt::MGMT_STORAGE).await,
        },
        MgmtOp::NodeUpdate { addr, name, flags } => {
            if name.is_some_and(|n| n.len() > mgmt::MAX_NODE_NAME) {
                respond_empty(req_id, mgmt::MGMT_BAD_ARG).await;
                return;
            }
            match registry::update(buckets, addr, name, flags) {
                Ok(()) => respond_empty(req_id, mgmt::MGMT_OK).await,
                Err(RegistryError::NotFound) => respond_empty(req_id, mgmt::MGMT_NOT_FOUND).await,
                Err(RegistryError::BadName) => respond_empty(req_id, mgmt::MGMT_BAD_ARG).await,
                Err(_) => respond_empty(req_id, mgmt::MGMT_STORAGE).await,
            }
        }
        MgmtOp::NodeRevealKey { addr } => match registry::find(buckets, addr) {
            // The one deliberate key-disclosure path (host asked explicitly; keys are
            // otherwise never emitted on any interface).
            Some(rec) => respond_record(req_id, &NodeKey { addr, key: rec.key }).await,
            None => respond_empty(req_id, mgmt::MGMT_NOT_FOUND).await,
        },
        // No key in the request: the link key comes from the X25519 key agreement (docs/radio.md).
        MgmtOp::PairingOpen { window } => {
            if pairing.is_some() {
                respond_empty(req_id, mgmt::MGMT_BUSY).await;
                return;
            }
            if window == 0 {
                respond_empty(req_id, mgmt::MGMT_BAD_ARG).await;
                return;
            }
            if registry::count(buckets) >= registry::CAPACITY {
                respond_empty(req_id, mgmt::MGMT_FULL).await;
                return;
            }
            // The window's first ephemeral (one X25519 scalar multiplication, ~1 s): done now, so
            // answering a JOIN_REQ costs only the shared secret. No TRNG, no pairing: an
            // ephemeral from the software fallback would be guessable (logged by the net layer).
            let Some(host) = PairingHost::prepare() else {
                respond_empty(req_id, mgmt::MGMT_UNSUPPORTED).await;
                return;
            };
            info!(target: "gateway", "pairing window open ({} s)", window);
            led.set_background(Some(PAIRING));
            *pairing = Some(PairingSession {
                req_id,
                host,
                deadline: Instant::now() + Duration::from_secs(window as u64),
            });
            // The response is DELAYED: it resolves when the window does.
        }
        MgmtOp::PairingCancel => {
            if let Some(sess) = pairing.take() {
                respond_empty(sess.req_id, mgmt::MGMT_TIMEOUT).await; // the open resolves…
                led.set_background(Some(HEARTBEAT));
            }
            respond_empty(req_id, mgmt::MGMT_OK).await; // …and the cancel acks
        }
        MgmtOp::QueuePush { node_addr, ttl, data } => {
            if registry::find(buckets, node_addr).is_none() {
                respond_empty(req_id, mgmt::MGMT_NOT_FOUND).await;
                return;
            }
            let ttl = if ttl == 0 { DEFAULT_TTL_S } else { ttl };
            match queue.push(node_addr, ttl, data, now_ms) {
                Ok(item) => {
                    // Pre-mark: the pending flag must ride the ACK of the node's NEXT
                    // uplink, which is built inside recv() before we see the frame.
                    net.set_pending(node_addr, true);
                    STAT_QUEUED.store(queue.len() as u32, Ordering::Relaxed);
                    respond_record(req_id, &QueueId { item }).await;
                }
                Err(PushError::BadPayload) => respond_empty(req_id, mgmt::MGMT_BAD_ARG).await,
                Err(_) => respond_empty(req_id, mgmt::MGMT_FULL).await,
            }
        }
        MgmtOp::QueueList { node_addr } => {
            let mut stream = ChunkStream::new(req_id);
            let mut scratch = [0u8; 128];
            for item in queue.iter() {
                if node_addr != 0 && item.node_addr != node_addr {
                    continue;
                }
                let entry = mgmt::QueueEntry {
                    node_addr: item.node_addr,
                    item: item.id,
                    age: item.age(now_ms),
                    ttl: item.ttl,
                    data: item.data(),
                };
                if let Ok(bytes) = postcard::to_slice(&entry, &mut scratch) {
                    stream.push(bytes).await;
                }
            }
            stream.finish().await;
        }
        MgmtOp::QueueDrop { node_addr, item } => {
            let dropped = match item {
                // Check the id→node binding BEFORE popping: a pop-then-re-push refusal
                // would re-mint the item under a fresh id (dangling the host's handle)
                // and move it to the back of its node's FIFO.
                Some(id) => {
                    let owner = queue.iter().find(|it| it.id == id).map(|it| it.node_addr);
                    if owner == Some(node_addr) {
                        queue.pop(id);
                        1
                    } else {
                        0
                    }
                }
                None => queue.drop_node(node_addr),
            };
            net.set_pending(node_addr, queue.count_for(node_addr) > 0);
            STAT_QUEUED.store(queue.len() as u32, Ordering::Relaxed);
            if dropped > 0 {
                respond_empty(req_id, mgmt::MGMT_OK).await;
            } else {
                respond_empty(req_id, mgmt::MGMT_NOT_FOUND).await;
            }
        }
        MgmtOp::StatsConfig { channel_period_ms } => {
            *stats_period_ms = channel_period_ms; // RAM-only override of the setting
            respond_empty(req_id, mgmt::MGMT_OK).await;
        }
        // Node-side ops: this device is a gateway.
        MgmtOp::Provision(_) | MgmtOp::JoinOpen { .. } => {
            warn!(target: "gateway", "unsupported mgmt op (node-side)");
            respond_empty(req_id, mgmt::MGMT_UNSUPPORTED).await;
        }
    }
}

app!(run, commands: GW_COMMANDS, settings: GW_SETTINGS);
