//! The gateway engine, a **pure, synchronous state machine**: `handle(Input) ->
//! Vec<Output>`. No I/O, no threads, no clocks beyond the injected 1 Hz `Tick`; the
//! surrounding threads (serial reader/writer, rumqttc connection loop, frontend) feed
//! `Input`s in and execute `Output`s. That seam is what makes the whole
//! serial↔MQTT bridge unit-testable without hardware or a broker: the same trick as
//! `session::Transport`, one level up.
//!
//! Responsibilities: mirror the dongle's registry into retained MQTT topics, translate
//! uplinks into telemetry topics (the gateway firmware forwards payloads verbatim;
//! *this* is where `tower_protocol::radio` is decoded), run the remote-shell
//! queue/response correlation (MQTT uuid ↔ radio `cmd_id` ↔ queue item), serve the
//! `gateway/cmd` RPC surface, stream radio diagnostics, and keep `gateway/status`
//! truthful across serial/MQTT link changes and gateway reboots (session_id).

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use tower_protocol::mgmt::{self, MgmtOp, NodeEntry, NodeKey, Paired, QueueEntry, QueueId};
use tower_protocol::msg::RadioStat;
use tower_protocol::radio::{self, NodeCmd, NodeMsg};

use crate::mgmt::{DeviceInfoOwned, NodeEntryOwned, encode_mgmt, parse_records};
use crate::session::Mismatch;

use super::payload::{self, PendingEntry, RpcRequest, RpcResponse};
use super::topics;

/// Idle ticks (1 Hz) before an in-flight management op is failed as a device timeout.
const MGMT_TIMEOUT_TICKS: u64 = 8;
/// Ticks a sent `PairingOpen` holds the dongle's request slot. The gateway receives management
/// requests through a one-deep inbox (RAM: every slot is a frame buffer) and takes the next one
/// at the top of its main loop, so a request sent before it took the previous one is dropped.
/// Every other op holds the slot until its answer, which comes after the gateway took it;
/// `PairingOpen` is answered only when the window ends, so it holds the slot just long enough
/// for one loop turn (a 250 ms receive slice plus the requests it serves) to take it.
const PAIRING_PICKUP_TICKS: u64 = 2;
/// Seconds a `PairingOpen` may resolve after its window (the RPC waits `window` + this). The
/// gateway's window runs from the request's arrival and it answers a JOIN_REQ only while a whole
/// handshake still fits, so a join completes inside the window; what can follow is the registry
/// commit absorbing a synchronous EEPROM compaction (~5.2 s worst case), or, for an expiry, the
/// loop turn that notices it (two management requests, one with such a compaction, ~6.5 s), the
/// console response, and this side's 1 s tick granularity. 15 s covers that with margin; a
/// `Paired` that still arrives later is reported as a late join, never dropped.
pub(crate) const PAIRING_SLACK_S: u64 = 15;
/// Idle ticks before a delivered-but-unanswered remote shell command is failed (the
/// node executed it on its wake cycle; a healthy reply arrives within its RX window).
const SHELL_REPLY_TIMEOUT_TICKS: u64 = 30;
/// Ticks between retained `gateway/stats` refreshes.
const STATS_EVERY_TICKS: u64 = 10;
/// The gateway firmware's queue TTL when a `ShellReq` leaves `ttl` at 0, in seconds.
const DEFAULT_QUEUE_TTL: u64 = 3600;
/// Slack past a queued (undelivered) shell command's TTL before the host gives up on it
/// without the gateway's `TX_EXPIRED` report: the backstop for a report lost to a missed
/// gateway reboot or a serial drop, so a pending entry can never live forever.
const SHELL_TTL_GRACE_TICKS: u64 = 30;
/// Largest command payload (`gateway/cmd`, `nodes/+/shell/req`) the engine will parse;
/// real requests are a few hundred bytes.
const MAX_COMMAND_PAYLOAD: usize = 4096;
/// How many recent MQTT request ids are remembered to drop QoS-1 redeliveries.
const RECENT_IDS: usize = 64;
/// Uplinks held (not dropped) while the startup registry sync is still in flight.
const HELD_UPLINKS: usize = 16;
/// `NodeList` answers in a row that may arrive with a lost chunk and still be listed again.
const LIST_GAP_RETRIES: u8 = 3;
/// Distinct unregistered senders tracked for drop accounting (the table resets beyond).
const UNREGISTERED_TRACKED: usize = 32;

/// One decoded target→host frame, owned (the serial thread copies out of its buffer).
#[derive(Debug, Clone)]
pub(crate) enum SerialMsg {
    Hello {
        firmware_name: String,
        firmware_version: String,
        session_id: u32,
    },
    Log {
        line: String,
    },
    Uplink {
        src: u32,
        counter: u32,
        rssi: i16,
        lqi: u8,
        data: Vec<u8>,
    },
    Mgmt {
        req_id: u16,
        result: u8,
        chunk: u16,
        last: bool,
        data: Vec<u8>,
    },
    Stat(RadioStat),
    Dropped(u32),
    /// Frames carrying another schema tag: the dongle now runs a firmware built from another
    /// checkout (a mid-session reflash). None of them is decoded; the serial thread forwards the
    /// first one and those naming the firmware (`serial::schema_gate`).
    Foreign(Mismatch),
}

/// Everything that can happen to the engine.
#[derive(Debug)]
pub(crate) enum Input {
    SerialUp,
    SerialDown {
        error: String,
    },
    SerialFrame(SerialMsg),
    MqttUp,
    MqttDown {
        error: String,
    },
    MqttIn {
        topic: String,
        payload: Vec<u8>,
        /// The broker replayed a *stored* retained message (live deliveries carry 0).
        retain: bool,
    },
    /// An in-process frontend action (the TUI drives the same surfaces the MQTT
    /// clients do). Its RPC responses come back as events, routed by this *origin*,
    /// never by the id string, which an MQTT client could mint to look like the TUI's.
    Frontend(FrontendCmd),
    /// 1 Hz housekeeping (timeouts, stats refresh, pairing countdown).
    Tick,
}

/// What the TUI can ask for.
#[derive(Debug)]
pub(crate) enum FrontendCmd {
    Rpc(RpcRequest),
    Shell { node: u32, req: payload::ShellReq },
}

/// Everything the engine wants done. Executed by the runner thread.
#[derive(Debug, PartialEq)]
pub(crate) enum Output {
    /// Write one pre-encoded frame to the dongle.
    SerialSend(Vec<u8>),
    Publish {
        topic: String,
        payload: Vec<u8>,
        retain: bool,
    },
    Subscribe(String),
    /// A frontend event (service log line / TUI view-model update).
    Event(Event),
}

/// Frontend feed: the service prints these; the TUI folds them into its view-model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    /// A human-readable line (device logs, engine milestones, warnings).
    Log(String),
    /// Link state changed.
    Link { serial_up: bool, mqtt_up: bool },
    /// The registry mirror changed (full snapshot: ≤16 nodes, cheap).
    Registry(Vec<NodeView>),
    /// A remote-shell response chunk (or terminal error) for a node's dialog.
    Shell { node: u32, rsp: payload::ShellRsp },
    /// One radio-graph sample.
    Radio(RadioSample),
    /// Pairing window state changed.
    Pairing(payload::Pairing),
    /// The response to a frontend-initiated RPC (`Input::Frontend` origin only).
    Rpc(RpcResponse),
    /// One MQTT message crossed the broker link (mirrored by the runner; the
    /// engine itself never emits this). TUI feed food; the service ignores it.
    Mqtt(MqttMsg),
}

/// Which way an MQTT message crossed the link, seen from the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MqttDir {
    /// Published by this gateway.
    Out,
    /// Received from a broker subscription (`gateway/cmd`, `nodes/+/shell/req`).
    In,
}

/// One mirrored MQTT message (the TUI's traffic-feed entry).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MqttMsg {
    pub dir: MqttDir,
    pub topic: String,
    pub payload: Vec<u8>,
    pub retain: bool,
}

/// The frontend's view of one node (mirror row + live pendings).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeView {
    pub addr: u32,
    pub name: String,
    pub kind: String,
    pub sleeping: bool,
    pub unnamed: bool,
    pub last_seen: Option<u32>,
    pub rssi: Option<i8>,
    pub uplinks: u32,
    pub queued: u8,
    pub pending: Vec<PendingEntry>,
}

/// One radio-graph sample (TUI chart food).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RadioSample {
    Ambient { dbm: i16, channel: u8 },
    Rx { src: u32, rssi: i16 },
    Tx { dest: u32, delivered: bool },
}

// ---- internal state ---------------------------------------------------------------

/// What an in-flight `MgmtRequest` was for (drives reply decoding + follow-ups).
#[derive(Debug, Clone)]
enum OpKind {
    /// `resync`: re-describe after a serial reconnect / gateway reboot.
    Describe,
    NodeList,
    NodeAdd {
        id: u32,
        name: String,
    },
    NodeRemove {
        id: u32,
    },
    /// A rename or a flags change of node `id`, mirrored when it is issued (`merge_registry`
    /// keeps that over a listing answered before it).
    NodeUpdate {
        id: u32,
    },
    RevealKey {
        id: u32,
    },
    PairingOpen,
    PairingCancel,
    QueuePush {
        node: u32,
        shell: Option<ShellOrigin>,
    },
    QueueList,
    /// Engine-internal `QueueList` reconciling the pending mirror with the device queue
    /// (startup purge of a previous host's leftovers; resync after a serial drop).
    QueueSync,
    QueueDrop {
        node: u32,
    },
    /// Engine-internal drop of one stale item found by [`OpKind::QueueSync`].
    QueueDropStale,
    StatsConfig,
}

/// Where a remote-shell command came from (for response routing).
#[derive(Debug, Clone)]
struct ShellOrigin {
    rpc: String,
    line: String,
    cmd_id: u16,
    /// Requested queue TTL (0 = the gateway default): the host-side expiry backstop.
    ttl: u16,
}

/// Which surface an RPC arrived on, and so where its response goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcOrigin {
    /// `gateway/cmd` → answered on `gateway/rsp/{id}`.
    Mqtt,
    /// The in-process TUI → answered as [`Event::Rpc`].
    Frontend,
}

/// An RPC awaiting its response: the client's correlation id + where to answer.
#[derive(Debug, Clone)]
struct RpcRef {
    id: String,
    origin: RpcOrigin,
}

/// One in-flight management request.
#[derive(Debug)]
struct PendingMgmt {
    req_id: u16,
    op: OpKind,
    /// The RPC to answer (None = engine-internal).
    rpc: Option<RpcRef>,
    data: Vec<u8>,
    next_chunk: u16,
    truncated: bool,
    /// Tick deadline (delayed ops, i.e. pairing, get their window + slack); `u64::MAX` while
    /// the op still waits for the request slot ([`Engine::send_next_mgmt`]).
    deadline: u64,
    /// The encoded request while it waits for the slot; `None` once sent.
    frame: Option<Vec<u8>>,
    /// Ticks from send to the answer's deadline.
    timeout_ticks: u64,
    /// Until this tick the sent request holds the dongle's one-deep request inbox
    /// (`u64::MAX`: until its answer).
    holds_slot_until: u64,
}

/// The registry mirror row (device truth + host-side enrichment).
#[derive(Debug, Clone)]
struct NodeMirror {
    entry: NodeEntryOwned,
    /// From the node's `NodeInfo` heartbeat: firmware name mapped to a kind.
    kind: String,
    /// Last `NodeInfo.session_id` (count-reset disambiguation for subscribers).
    session_id: Option<u32>,
    /// Live remote-shell queue entries (ref = gateway queue item id).
    pending: Vec<PendingShell>,
    /// The highest radio counter of this node's uplinks published by this process (`None`
    /// until the first, and again after a `node_add` re-key). An uplink at or below it is a
    /// replay and is dropped: see [`Engine::on_uplink`].
    last_counter: Option<u32>,
}

#[derive(Debug, Clone)]
struct PendingShell {
    item: u16,
    rpc: String,
    line: String,
    cmd_id: u16,
    /// Set once the gateway reports TX_DELIVERED (awaiting the node's reply chunks).
    delivered_tick: Option<u64>,
    /// While undelivered: the tick past which the host stops waiting for the gateway's
    /// `TX_EXPIRED` report (queue TTL + grace).
    expires_tick: u64,
    next_chunk: u16,
    truncated: bool,
}

/// An uplink parked until the startup registry sync says whether its sender is ours.
#[derive(Debug)]
struct HeldUplink {
    src: u32,
    counter: u32,
    rssi: i16,
    lqi: u8,
    data: Vec<u8>,
}

pub(crate) struct Engine {
    prefix: String,
    port_name: String,
    describe: DeviceInfoOwned,
    hello_session: Option<u32>,
    serial_up: bool,
    mqtt_up: bool,
    seq: u16,
    next_req: u16,
    next_cmd: u16,
    /// Per-process random dedup epoch stamped on every downlink shell command.
    shell_epoch: u32,
    pending_mgmt: Vec<PendingMgmt>,
    nodes: Vec<NodeMirror>,
    /// The first `NodeList` has been merged; before that an unknown uplink sender may
    /// simply not be mirrored *yet* (held, not dropped).
    registry_synced: bool,
    /// `NodeList` answers in a row that arrived with a lost chunk (not merged).
    list_gaps: u8,
    held: Vec<HeldUplink>,
    /// Uplinks dropped per unregistered sender (bounded table).
    unregistered: Vec<(u32, u64)>,
    /// Recently seen MQTT request ids (QoS-1 redelivery dedup), oldest first.
    recent_ids: VecDeque<String>,
    /// The `req_id` of the `PairingOpen` whose window `gateway/pairing` shows (with its end
    /// tick in `pairing_until`). Only that request's outcome closes it: a `pairing_cancel` lets
    /// go of it before the dongle answers the cancelled request, whose answer must not close
    /// another window.
    pairing_req: Option<u16>,
    pairing_until: Option<u64>,
    /// The `req_id` of a `PairingOpen` this side gave up on (timed out): its answer may still
    /// come, and a late `Paired` is a node the gateway did register.
    late_pairing: Option<u16>,
    tick: u64,
    uplinks: u64,
    last_ambient: Option<(i16, u8)>,
}

/// Is `name` acceptable as a node name? Printable text only: names are echoed on plain
/// terminal output (`nodes list`, service logs), so control characters, the bidi
/// embedding/override/isolate controls and anything else non-printable are refused at
/// the source. Length (1..=`MAX_NODE_NAME` bytes) is the caller's check.
fn printable_name(name: &str) -> bool {
    name.chars()
        .all(|c| !c.is_control() && !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
}

/// Parse a 32-hex-digit AES key.
fn parse_key_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut key = [0u8; 16];
    for (i, k) in key.iter_mut().enumerate() {
        *k = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(key)
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn json<T: serde::Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

/// Firmware name → device kind: `radio_push_button` → `push-button`.
fn kind_of(firmware_name: &str) -> String {
    firmware_name
        .strip_prefix("radio_")
        .unwrap_or(firmware_name)
        .replace('_', "-")
}

/// The auto-name `kind:n` (`push-button:0`), the kind clipped on a char boundary so the whole
/// name fits [`mgmt::MAX_NODE_NAME`]: the dongle refuses a longer one, and the node, still
/// unnamed, would be named again (and refused again) on every heartbeat. A clipped kind can
/// match another one's, but the caller's search for a free `n` keeps the name unique.
fn auto_name(kind: &str, n: u32) -> String {
    let suffix = format!(":{n}");
    let mut end = kind.len().min(mgmt::MAX_NODE_NAME.saturating_sub(suffix.len()));
    while !kind.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &kind[..end])
}

impl Engine {
    pub(crate) fn new(prefix: String, port_name: String, describe: DeviceInfoOwned) -> Self {
        Self {
            prefix,
            port_name,
            describe,
            hello_session: None,
            serial_up: true,
            mqtt_up: false,
            seq: 0,
            next_req: 1,
            next_cmd: 1,
            shell_epoch: 0,
            pending_mgmt: Vec::new(),
            nodes: Vec::new(),
            registry_synced: false,
            list_gaps: 0,
            held: Vec::new(),
            unregistered: Vec::new(),
            recent_ids: VecDeque::new(),
            pairing_req: None,
            pairing_until: None,
            late_pairing: None,
            tick: 0,
            uplinks: 0,
            last_ambient: None,
        }
    }

    /// Seed a fresh random per-process **shell dedup epoch** and **starting `cmd_id`**.
    ///
    /// The node de-duplicates the gateway's at-least-once re-deliveries on the
    /// `(epoch, cmd_id)` pair, and its memory survives a host restart (the node
    /// keeps running). Reusing a low `cmd_id` from a fresh host would otherwise be
    /// mistaken for a re-delivery the node already ran: the "checkmark, no output"
    /// bug. A 32-bit random epoch per `tower gateway` process makes that collision
    /// ~2⁻³² (a fresh host would have to draw the previous process's exact epoch AND
    /// reuse its `cmd_id`).
    ///
    /// The node's *reply* carries only the `cmd_id`, though, and the gateway's queue
    /// (TTL up to an hour) survives a host restart: a late reply to the previous
    /// process's command #1 would be credited to this process's command #1. A random
    /// starting `cmd_id` makes that ~1/65535 per pending command; the startup
    /// [`OpKind::QueueSync`] purge removes the leftovers outright. Production only;
    /// tests keep the deterministic epoch 0 / first `cmd_id` 1.
    pub(crate) fn randomize_shell_ids(&mut self) {
        let mut b = [0u8; 6];
        if getrandom::fill(&mut b).is_ok() {
            self.shell_epoch = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            self.next_cmd = u16::from_le_bytes([b[4], b[5]]).max(1);
        }
    }

    /// The initial output batch: subscriptions + a registry/queue sync. The queue sync
    /// purges items a previous host process left queued on the gateway; their replies
    /// could only be mis-credited (see [`Engine::randomize_shell_ids`]).
    pub(crate) fn start(&mut self) -> Vec<Output> {
        let mut out = vec![
            Output::Subscribe(topics::gateway_cmd(&self.prefix)),
            Output::Subscribe(topics::shell_req_filter(&self.prefix)),
        ];
        self.issue(&mut out, OpKind::NodeList, MgmtOp::NodeList, None);
        self.issue(
            &mut out,
            OpKind::QueueSync,
            MgmtOp::QueueList { node_addr: 0 },
            None,
        );
        out.push(Output::Event(Event::Log(format!(
            "gateway {} on {} ({} node slots)",
            topics::node_hex(self.describe.addr),
            self.port_name,
            self.describe.node_capacity
        ))));
        out
    }

    pub(crate) fn handle(&mut self, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        match input {
            Input::SerialUp => {
                self.serial_up = true;
                self.publish_status(&mut out);
                out.push(Output::Event(Event::Link {
                    serial_up: true,
                    mqtt_up: self.mqtt_up,
                }));
                // Resync: the dongle may have rebooted while we were away (its Hello
                // settles that), and requests issued while the link was down were never
                // delivered: reconcile the pending mirror with what is really queued, and the
                // registry mirror with the device's (it took a rename or an auto-name when it
                // was queued, which the drop may have failed unsent).
                self.issue(&mut out, OpKind::Describe, MgmtOp::Describe, None);
                self.issue(
                    &mut out,
                    OpKind::QueueSync,
                    MgmtOp::QueueList { node_addr: 0 },
                    None,
                );
                self.issue(&mut out, OpKind::NodeList, MgmtOp::NodeList, None);
            }
            Input::SerialDown { error } => {
                self.serial_up = false;
                self.publish_status(&mut out);
                out.push(Output::Event(Event::Link {
                    serial_up: false,
                    mqtt_up: self.mqtt_up,
                }));
                out.push(Output::Event(Event::Log(format!("serial lost: {error}"))));
                self.fail_unsent_mgmt(&mut out);
            }
            Input::SerialFrame(msg) => self.on_serial(msg, &mut out),
            Input::MqttUp => {
                self.mqtt_up = true;
                out.push(Output::Subscribe(topics::gateway_cmd(&self.prefix)));
                out.push(Output::Subscribe(topics::shell_req_filter(&self.prefix)));
                // Retained world re-publish: reconnect-resync is free by design.
                self.publish_status(&mut out);
                self.publish_stats(&mut out);
                self.publish_pairing(&mut out, None, None);
                for i in 0..self.nodes.len() {
                    self.publish_node(&mut out, i);
                    self.publish_pending(&mut out, i);
                }
                out.push(Output::Event(Event::Link {
                    serial_up: self.serial_up,
                    mqtt_up: true,
                }));
            }
            Input::MqttDown { error } => {
                // Also reported while never connected (each distinct error once): a
                // wrong password or unreachable broker must not fail silently.
                let line = if self.mqtt_up {
                    format!("mqtt lost: {error}")
                } else {
                    format!("mqtt: cannot connect to the broker: {error}")
                };
                self.mqtt_up = false;
                out.push(Output::Event(Event::Link {
                    serial_up: self.serial_up,
                    mqtt_up: false,
                }));
                out.push(Output::Event(Event::Log(line)));
            }
            Input::MqttIn {
                topic,
                payload,
                retain,
            } => self.on_mqtt(&topic, &payload, retain, &mut out),
            Input::Frontend(FrontendCmd::Rpc(req)) => self.on_rpc(req, RpcOrigin::Frontend, &mut out),
            Input::Frontend(FrontendCmd::Shell { node, req }) => self.on_shell_req(node, req, &mut out),
            Input::Tick => self.on_tick(&mut out),
        }
        out
    }

    // ---- serial → engine ------------------------------------------------------

    fn on_serial(&mut self, msg: SerialMsg, out: &mut Vec<Output>) {
        match msg {
            SerialMsg::Hello {
                firmware_name,
                firmware_version,
                session_id,
            } => {
                // The dongle emits Hello once per console session: at boot (it is
                // USB-powered: a replug is a reboot too). The boot Hello of the session we
                // attached to was consumed by the startup `describe` probe (and DeviceInfo
                // carries no session id), so NO session is pinned at start: the first
                // Hello seen here already means a reboot since attach. Only a Hello that
                // repeats the pinned session (a console restart without a reboot) is benign.
                let rebooted = self.hello_session != Some(session_id);
                self.hello_session = Some(session_id);
                self.describe.firmware_version = firmware_version;
                self.describe.firmware_name = firmware_name;
                if rebooted {
                    out.push(Output::Event(Event::Log(format!(
                        "gateway rebooted (session {session_id}): resyncing; queued downlinks were lost"
                    ))));
                    // The RAM queue died with the old session: fail every pending
                    // command loudly, then resync the registry.
                    for i in 0..self.nodes.len() {
                        let node = self.nodes[i].entry.addr;
                        for p in std::mem::take(&mut self.nodes[i].pending) {
                            self.shell_error(out, node, &p, "gateway rebooted; command lost");
                        }
                        self.publish_pending(out, i);
                    }
                    self.issue(out, OpKind::Describe, MgmtOp::Describe, None);
                    self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
                }
                self.publish_status(out);
            }
            SerialMsg::Log { line } => out.push(Output::Event(Event::Log(line))),
            SerialMsg::Uplink {
                src,
                counter,
                rssi,
                lqi,
                data,
            } => self.on_uplink(
                HeldUplink {
                    src,
                    counter,
                    rssi,
                    lqi,
                    data,
                },
                out,
            ),
            SerialMsg::Mgmt {
                req_id,
                result,
                chunk,
                last,
                data,
            } => self.on_mgmt_chunk(req_id, result, chunk, last, data, out),
            SerialMsg::Stat(stat) => self.on_radio_stat(stat, out),
            SerialMsg::Dropped(count) => {
                out.push(Output::Event(Event::Log(format!(
                    "gateway console dropped {count} frame(s) (link congestion)"
                ))));
            }
            SerialMsg::Foreign(m) => {
                out.push(Output::Event(Event::Log(format!(
                    "SCHEMA MISMATCH (dongle reflashed?): {}; {}; its frames are ignored \
                     until a firmware from this checkout is back",
                    m.what(),
                    m.remedy()
                ))));
            }
        }
    }

    /// One forwarded uplink. Only **registered** nodes reach MQTT: the host is the last
    /// line before the public topic tree, so a sender the registry doesn't know (a stale
    /// mirror aside) is dropped and counted, not published, whatever let it through the
    /// gateway. Before the first registry sync the mirror is simply empty, so those
    /// uplinks are parked and replayed once it lands.
    ///
    /// **Replays across a gateway reboot.** The dongle persists each node's replay lane only
    /// every 32 accepted frames (EEPROM wear), so after a reboot it accepts up to 31 of a
    /// node's already-seen counters once more: recorded uplinks an attacker replays would be
    /// published as new events. The host keeps the node's highest published counter and drops
    /// anything at or below it. A node's counter only grows (it jumps to its next reserved block
    /// across a node reboot), so nothing genuine is lost; a re-keyed node (a `node_add`, a new
    /// pairing) starts over. RAM-only: a host restarted together with the dongle has no record
    /// (docs/security-model.md, 4.1).
    fn on_uplink(&mut self, u: HeldUplink, out: &mut Vec<Output>) {
        let Some(node) = self.nodes.iter_mut().find(|n| n.entry.addr == u.src) else {
            if !self.registry_synced {
                if self.held.len() < HELD_UPLINKS {
                    self.held.push(u);
                }
                return;
            }
            // Keep the radio-graph mark (something authenticated transmitted); it
            // never leaves the process.
            out.push(Output::Event(Event::Radio(RadioSample::Rx {
                src: u.src,
                rssi: u.rssi,
            })));
            return self.drop_unregistered(u.src, out);
        };
        if let Some(last) = node.last_counter.filter(|&last| u.counter <= last) {
            out.push(Output::Event(Event::Log(format!(
                "dropped uplink from {}: counter {} <= {last} already published (a replay, e.g. after a gateway reboot)",
                topics::node_hex(u.src),
                u.counter
            ))));
            return;
        }
        node.last_counter = Some(u.counter);
        let HeldUplink {
            src,
            counter,
            rssi,
            lqi,
            data,
        } = u;
        self.uplinks += 1;
        self.touch_node(src, rssi);
        out.push(Output::Event(Event::Radio(RadioSample::Rx { src, rssi })));
        out.push(Output::Publish {
            topic: topics::radio_rx(&self.prefix),
            payload: json(&payload::RadioRx {
                src: topics::node_hex(src),
                rssi,
                lqi,
                len: data.len(),
                ts: now_ts(),
            }),
            retain: false,
        });
        out.push(Output::Publish {
            topic: topics::node_uplink(&self.prefix, src),
            payload: json(&payload::UplinkDebug {
                counter,
                rssi,
                lqi,
                len: data.len(),
                hex: data.iter().map(|b| format!("{b:02x}")).collect(),
                ts: now_ts(),
            }),
            retain: false,
        });
        self.on_node_msg(src, counter, &data, out);
    }

    /// Account one uplink from a sender the registry doesn't know: log at 1, 10, 100, …
    /// drops per sender (bounded noise), and on the first one re-list the registry in case
    /// the mirror is merely stale.
    fn drop_unregistered(&mut self, src: u32, out: &mut Vec<Output>) {
        let count = match self.unregistered.iter_mut().find(|(a, _)| *a == src) {
            Some((_, c)) => {
                *c += 1;
                *c
            }
            None => {
                if self.unregistered.len() >= UNREGISTERED_TRACKED {
                    self.unregistered.clear();
                }
                self.unregistered.push((src, 1));
                self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
                1
            }
        };
        let mut n = count;
        while n % 10 == 0 {
            n /= 10;
        }
        if n == 1 {
            out.push(Output::Event(Event::Log(format!(
                "dropped uplink from unregistered node {} ({count} so far): not in the gateway registry, not published",
                topics::node_hex(src)
            ))));
        }
    }

    /// Decode one forwarded radio-application payload: the host side of the
    /// "gateway is a transparent bridge" contract.
    fn on_node_msg(&mut self, src: u32, counter: u32, data: &[u8], out: &mut Vec<Output>) {
        let ts = now_ts();
        match radio::decode_node_msg(data) {
            Ok(NodeMsg::Button { kind, count }) => {
                let event = match kind {
                    radio::ButtonKind::Press => "press",
                    radio::ButtonKind::Release => "release",
                    radio::ButtonKind::Click => "click",
                    radio::ButtonKind::Hold => "hold",
                };
                out.push(Output::Publish {
                    topic: topics::node_button(&self.prefix, src),
                    payload: json(&payload::ButtonEvent {
                        event: event.into(),
                        count,
                        counter,
                        ts,
                    }),
                    retain: false,
                });
            }
            Ok(NodeMsg::Temperature { millic }) => {
                out.push(Output::Publish {
                    topic: topics::node_temperature(&self.prefix, src),
                    payload: json(&payload::Temperature {
                        celsius: millic as f64 / 1000.0,
                        millic,
                        ts,
                    }),
                    retain: true,
                });
            }
            Ok(NodeMsg::Accel { kind, face }) => {
                let event = match kind {
                    radio::AccelKind::Motion => "motion",
                    radio::AccelKind::Orientation => "orientation",
                };
                out.push(Output::Publish {
                    topic: topics::node_accel(&self.prefix, src),
                    payload: json(&payload::AccelEvent {
                        event: event.into(),
                        face,
                        counter,
                        ts,
                    }),
                    retain: false,
                });
            }
            Ok(NodeMsg::Info(info)) => self.on_node_info(src, &info, out),
            Ok(NodeMsg::Shell(chunk)) => self.on_shell_chunk(src, &chunk, out),
            Err(tower_protocol::Error::SchemaMismatch { got }) => {
                out.push(Output::Event(Event::Log(format!(
                    "node {} speaks radio schema {got:#06x}, this `tower` {:#06x}; rebuild both \
                     from the same checkout",
                    topics::node_hex(src),
                    radio::RADIO_TAG
                ))));
            }
            Err(_) => {
                out.push(Output::Event(Event::Log(format!(
                    "node {}: undecodable application payload ({} B)",
                    topics::node_hex(src),
                    data.len()
                ))));
            }
        }
    }

    /// A `NodeInfo` heartbeat: enrich the mirror (kind/sleeping) and auto-name
    /// unnamed nodes, `push-button:0` style, host-side by design (the gateway
    /// firmware never interprets payloads).
    fn on_node_info(&mut self, src: u32, info: &radio::NodeInfo<'_>, out: &mut Vec<Output>) {
        let kind = kind_of(info.firmware_name);
        let Some(i) = self.nodes.iter().position(|n| n.entry.addr == src) else {
            return; // unregistered node: forwarded but not ours to manage
        };
        self.nodes[i].kind = kind.clone();
        self.nodes[i].session_id = Some(info.session_id);

        let mut flags = self.nodes[i].entry.flags;
        if info.sleeping {
            flags |= mgmt::NODE_FLAG_SLEEPING;
        } else {
            flags &= !mgmt::NODE_FLAG_SLEEPING;
        }

        // The mirror takes an update only once it is queued: one refused up front (the serial
        // link is down) never reaches the dongle, and the next heartbeat tries again.
        if self.nodes[i].entry.flags & mgmt::NODE_FLAG_UNNAMED != 0 {
            // Auto-name: smallest free index for this kind.
            let name = (0..)
                .map(|n| auto_name(&kind, n))
                .find(|cand| !self.nodes.iter().any(|m| &m.entry.name == cand))
                .unwrap();
            flags &= !mgmt::NODE_FLAG_UNNAMED;
            out.push(Output::Event(Event::Log(format!(
                "auto-naming node {} \"{name}\"",
                topics::node_hex(src)
            ))));
            // `op` borrows the local; `issue` encodes it immediately, so no leak/copy.
            let op = MgmtOp::NodeUpdate {
                addr: src,
                name: Some(name.as_str()),
                flags: Some(flags),
            };
            if self
                .issue(out, OpKind::NodeUpdate { id: src }, op, None)
                .is_some()
            {
                self.nodes[i].entry.name = name;
                self.nodes[i].entry.flags = flags;
            }
        } else if flags != self.nodes[i].entry.flags {
            let op = MgmtOp::NodeUpdate {
                addr: src,
                name: None,
                flags: Some(flags),
            };
            if self
                .issue(out, OpKind::NodeUpdate { id: src }, op, None)
                .is_some()
            {
                self.nodes[i].entry.flags = flags;
            }
        }
        self.publish_node(out, i);
    }

    /// One remote-shell response chunk from a node → the MQTT dialog + frontend.
    fn on_shell_chunk(&mut self, src: u32, chunk: &radio::NodeShellChunk<'_>, out: &mut Vec<Output>) {
        let Some(i) = self.nodes.iter().position(|n| n.entry.addr == src) else {
            return;
        };
        let Some(pi) = self.nodes[i]
            .pending
            .iter()
            .position(|p| p.cmd_id == chunk.cmd_id)
        else {
            out.push(Output::Event(Event::Log(format!(
                "node {}: shell chunk for unknown cmd_id {}",
                topics::node_hex(src),
                chunk.cmd_id
            ))));
            return;
        };
        let gap = self.nodes[i].pending[pi].next_chunk != chunk.chunk;
        if gap {
            self.nodes[i].pending[pi].truncated = true;
        }
        self.nodes[i].pending[pi].next_chunk = chunk.chunk.wrapping_add(1);
        // The reply timeout counts from the last sign of life: a long streamed reply is cut
        // only once the node goes quiet. A chunk also proves the command delivered, should its
        // `TX_DELIVERED` report come later (or be lost).
        self.nodes[i].pending[pi].delivered_tick = Some(self.tick);
        let rsp = payload::ShellRsp {
            id: self.nodes[i].pending[pi].rpc.clone(),
            chunk: chunk.chunk,
            text: chunk.text.to_string(),
            done: chunk.last,
            result: chunk.result,
            truncated: self.nodes[i].pending[pi].truncated,
            error: None,
        };
        out.push(Output::Publish {
            topic: topics::node_shell_rsp(&self.prefix, src),
            payload: json(&rsp),
            retain: false,
        });
        out.push(Output::Event(Event::Shell { node: src, rsp }));
        if chunk.last {
            self.nodes[i].pending.remove(pi);
            self.publish_pending(out, i);
        }
    }

    fn on_radio_stat(&mut self, stat: RadioStat, out: &mut Vec<Output>) {
        let ts = now_ts();
        match stat {
            RadioStat::Channel { channel, rssi } => {
                self.last_ambient = Some((rssi, channel));
                out.push(Output::Event(Event::Radio(RadioSample::Ambient {
                    dbm: rssi,
                    channel,
                })));
                out.push(Output::Publish {
                    topic: topics::radio_rssi(&self.prefix),
                    payload: json(&payload::RadioRssi {
                        dbm: rssi,
                        channel,
                        ts,
                    }),
                    retain: false,
                });
            }
            RadioStat::Tx {
                dest,
                item,
                outcome,
                ack_rssi,
            } => {
                out.push(Output::Event(Event::Radio(RadioSample::Tx {
                    dest,
                    delivered: outcome == mgmt::TX_DELIVERED,
                })));
                out.push(Output::Publish {
                    topic: topics::radio_tx(&self.prefix),
                    payload: json(&payload::RadioTx {
                        dest: topics::node_hex(dest),
                        item,
                        outcome: payload::tx_outcome_str(outcome).into(),
                        ack_rssi,
                        ts,
                    }),
                    retain: false,
                });
                if item != 0 {
                    self.on_queue_outcome(dest, item, outcome, out);
                }
            }
        }
    }

    /// A queue item left the gateway (delivered) or died (expired / kept for retry).
    fn on_queue_outcome(&mut self, node: u32, item: u16, outcome: u8, out: &mut Vec<Output>) {
        let Some(i) = self.nodes.iter().position(|n| n.entry.addr == node) else {
            return;
        };
        match outcome {
            mgmt::TX_DELIVERED => {
                if let Some(p) = self.nodes[i].pending.iter_mut().find(|p| p.item == item) {
                    p.delivered_tick = Some(self.tick);
                }
                // Stays in `pending` until the node's reply completes (the dialog's
                // "pending" marker covers queued *and* awaiting-reply).
            }
            mgmt::TX_EXPIRED => {
                if let Some(pi) = self.nodes[i].pending.iter().position(|p| p.item == item) {
                    let p = self.nodes[i].pending.remove(pi);
                    // An item whose transmit went unacknowledged (the node may have run it, only its ACK
                    // lost) lapses the same way: say what is known.
                    self.shell_error(out, node, &p, "queue TTL expired without a confirmed delivery");
                    self.publish_pending(out, i);
                }
            }
            _ => {} // not-delivered/busy/duty: the gateway retries on the next uplink
        }
    }

    // ---- management replies -----------------------------------------------------

    fn on_mgmt_chunk(
        &mut self,
        req_id: u16,
        result: u8,
        chunk: u16,
        last: bool,
        data: Vec<u8>,
        out: &mut Vec<Output>,
    ) {
        let Some(idx) = self.pending_mgmt.iter().position(|p| p.req_id == req_id) else {
            if last && self.late_pairing == Some(req_id) {
                self.late_pairing = None;
                self.on_late_pairing(result, &data, out);
            }
            return; // otherwise stale/unknown (e.g. a timed-out op answering late)
        };
        {
            let p = &mut self.pending_mgmt[idx];
            if p.next_chunk != chunk {
                p.truncated = true;
            }
            p.next_chunk = chunk.wrapping_add(1);
            p.data.extend_from_slice(&data);
        }
        if !last {
            return;
        }
        let done = self.pending_mgmt.remove(idx);
        self.on_mgmt_done(done, result, out);
        self.send_next_mgmt(out);
    }

    fn on_mgmt_done(&mut self, p: PendingMgmt, result: u8, out: &mut Vec<Output>) {
        let err = payload::mgmt_error_str(result);
        match (&p.op, err) {
            // ---- happy paths ----
            (OpKind::Describe, None) => {
                if let Ok(info) = postcard::from_bytes::<tower_protocol::mgmt::DeviceInfo>(&p.data) {
                    // A reply that decoded carried our schema tag (a foreign one is never
                    // decoded). Carries `firmware_version` too, so the status no longer depends
                    // on having seen a boot Hello.
                    self.describe = DeviceInfoOwned::from(info);
                    self.publish_status(out);
                }
                // Band + channel are what a cable-provisioned node must be told
                // (`nodes add --port`): without them it was provisioned onto band 0 /
                // channel 0 whatever the gateway ran, unreachable on us915 or ch 1/2.
                self.rpc_ok(
                    out,
                    &p,
                    serde_json::json!({
                        "gateway": topics::node_hex(self.describe.addr),
                        "band": self.describe.band,
                        "channel": self.describe.channel,
                    }),
                );
            }
            (OpKind::NodeList, None) if p.truncated => {
                // A lost chunk: the records around the gap are spliced from two halves, and a
                // node missing from the list proves nothing. Merging would drop live nodes
                // (clearing their retained topics, ending their dialogs) or mirror garbage, so
                // the mirror stays as it is (and the startup sync pending) and the registry is
                // listed again: a lost chunk is a transient. A few times in a row at most, so a
                // link that keeps losing them can't loop; the next refresh tries again.
                self.rpc_err(
                    out,
                    &p,
                    "the device's answer arrived incomplete (a chunk was lost)",
                );
                self.list_gaps = self.list_gaps.saturating_add(1);
                if self.list_gaps <= LIST_GAP_RETRIES
                    && !self.pending_mgmt.iter().any(|q| matches!(q.op, OpKind::NodeList))
                {
                    self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
                }
            }
            (OpKind::NodeList, None) => {
                self.list_gaps = 0;
                let entries: Vec<NodeEntry> = parse_records(&p.data);
                let owned: Vec<NodeEntryOwned> = entries.into_iter().map(Into::into).collect();
                self.merge_registry(owned, out);
                let data = serde_json::to_value(self.node_payloads()).unwrap_or_default();
                self.rpc_ok(out, &p, data);
                if !self.registry_synced {
                    // The startup sync landed: replay what arrived before it (still
                    // unregistered senders are dropped now).
                    self.registry_synced = true;
                    for u in std::mem::take(&mut self.held) {
                        self.on_uplink(u, out);
                    }
                }
            }
            (OpKind::NodeAdd { id, name }, None) => {
                out.push(Output::Event(Event::Log(format!(
                    "node {} added ({name})",
                    topics::node_hex(*id)
                ))));
                let flags = if name.is_empty() {
                    mgmt::NODE_FLAG_UNNAMED
                } else {
                    0
                };
                self.provisional_node(*id, name, flags);
                // A (re-)added key may come with a counter that restarted: forget the old one.
                if let Some(n) = self.nodes.iter_mut().find(|n| n.entry.addr == *id) {
                    n.last_counter = None;
                }
                self.rpc_ok(out, &p, serde_json::Value::Null);
                self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
            }
            (OpKind::NodeRemove { id }, None) => {
                let id = *id;
                let pending = match self.nodes.iter().position(|n| n.entry.addr == id) {
                    Some(i) => self.nodes.remove(i).pending,
                    None => Vec::new(),
                };
                self.node_gone(out, id, &pending);
                self.emit_registry(out);
                self.rpc_ok(out, &p, serde_json::Value::Null);
            }
            (OpKind::NodeUpdate { .. }, None) => {
                self.rpc_ok(out, &p, serde_json::Value::Null);
                self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
            }
            (OpKind::RevealKey { id }, None) => match postcard::from_bytes::<NodeKey>(&p.data) {
                Ok(k) => {
                    let hex: String = k.key.iter().map(|b| format!("{b:02x}")).collect();
                    // The counter floors travel with the key: a `node_add` of it elsewhere
                    // passes them back (the firmware's NodeAdd docs).
                    self.rpc_ok(
                        out,
                        &p,
                        serde_json::json!({
                            "addr": topics::node_hex(*id),
                            "key": hex,
                            "tx_floor": k.tx_floor,
                            "rx_floor": k.rx_floor,
                        }),
                    );
                }
                Err(_) => self.rpc_err(out, &p, "malformed key record"),
            },
            (OpKind::PairingOpen, None) => {
                // Delayed resolution: a node joined (`on_paired` publishes the join).
                let ours = self.pairing_req == Some(p.req_id);
                if ours {
                    self.pairing_req = None;
                    self.pairing_until = None;
                }
                let joined = postcard::from_bytes::<Paired>(&p.data).ok();
                match joined {
                    Some(j) => {
                        let (hex, sas) = self.on_paired(j, false, out);
                        self.rpc_ok(out, &p, serde_json::json!({ "joined": hex, "sas": sas }));
                    }
                    None => {
                        out.push(Output::Event(Event::Log("pairing window resolved".into())));
                        if ours {
                            self.publish_pairing(out, None, None);
                        }
                        self.rpc_ok(out, &p, serde_json::json!({ "joined": null }));
                        self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
                    }
                }
            }
            (OpKind::PairingCancel, None) => self.rpc_ok(out, &p, serde_json::Value::Null),
            (OpKind::QueuePush { node, shell }, None) => {
                let node = *node;
                match postcard::from_bytes::<QueueId>(&p.data) {
                    Ok(qid) => {
                        if let Some(origin) = shell
                            && let Some(i) = self.nodes.iter().position(|n| n.entry.addr == node)
                        {
                            let ttl = match origin.ttl {
                                0 => DEFAULT_QUEUE_TTL,
                                t => t as u64,
                            };
                            self.nodes[i].pending.push(PendingShell {
                                item: qid.item,
                                rpc: origin.rpc.clone(),
                                line: origin.line.clone(),
                                cmd_id: origin.cmd_id,
                                delivered_tick: None,
                                expires_tick: self.tick + ttl + SHELL_TTL_GRACE_TICKS,
                                next_chunk: 0,
                                truncated: false,
                            });
                            self.publish_pending(out, i);
                            self.publish_node(out, i);
                        }
                        self.rpc_ok(out, &p, serde_json::json!({ "ref": qid.item }));
                    }
                    Err(_) => self.rpc_err(out, &p, "malformed queue id"),
                }
            }
            (OpKind::QueueList, None) => {
                let entries: Vec<QueueEntry> = parse_records(&p.data);
                let list: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "addr": topics::node_hex(e.node_addr),
                            "ref": e.item,
                            "age": e.age,
                            "ttl": e.ttl,
                        })
                    })
                    .collect();
                self.rpc_ok(out, &p, serde_json::Value::Array(list));
            }
            (OpKind::QueueSync, None) => {
                // A truncated list can't prove an item absent: skip rather than fail
                // live commands on a chunk loss; the next resync tries again.
                if !p.truncated {
                    let entries: Vec<QueueEntry> = parse_records(&p.data);
                    let device: Vec<(u32, u16)> = entries.iter().map(|e| (e.node_addr, e.item)).collect();
                    self.reconcile_queue(&device, out);
                }
            }
            (OpKind::QueueDrop { node }, None) => {
                let node = *node;
                // The dequeue succeeded on the device; drop the mirror entry too. We
                // don't know which item without echoing it; the caller's ref is in
                // the RPC params, so re-list pending from the mirror by pruning the
                // ref recorded at issue time (see rpc handling; the ref rides in op).
                self.rpc_ok(out, &p, serde_json::Value::Null);
                if let Some(i) = self.nodes.iter().position(|n| n.entry.addr == node) {
                    self.publish_pending(out, i);
                }
                self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
            }
            (OpKind::QueueDropStale, _) => {} // best effort; the item's TTL is the backstop
            (OpKind::StatsConfig, None) => self.rpc_ok(out, &p, serde_json::Value::Null),
            // ---- refusals ----
            (OpKind::PairingOpen, Some(e)) => {
                self.end_pairing(out, p.req_id);
                if result == mgmt::MGMT_TIMEOUT {
                    // Window expired unjoined: a normal outcome, not an error.
                    self.rpc_ok(out, &p, serde_json::json!({ "joined": null }));
                } else {
                    self.rpc_err(out, &p, e);
                }
            }
            (OpKind::QueuePush { node, shell }, Some(e)) => {
                let node = *node;
                if let Some(origin) = shell {
                    // The shell's generic failure, as for every bridge-reported end: the
                    // dongle's management code is another code space (`MGMT_FULL` = 4 and
                    // `MGMT_BUSY` = 5 would read as shell results; 5 is the shell's "denied").
                    // The reason is in `error`.
                    let rsp = payload::ShellRsp {
                        id: origin.rpc.clone(),
                        chunk: 0,
                        text: String::new(),
                        done: true,
                        result: 1,
                        truncated: false,
                        error: Some(e.to_string()),
                    };
                    out.push(Output::Publish {
                        topic: topics::node_shell_rsp(&self.prefix, node),
                        payload: json(&rsp),
                        retain: false,
                    });
                    out.push(Output::Event(Event::Shell { node, rsp }));
                }
                self.rpc_err(out, &p, e);
            }
            (OpKind::QueueSync, Some(e)) => out.push(Output::Event(Event::Log(format!(
                "queue resync refused by the gateway: {e}"
            )))),
            (OpKind::NodeUpdate { .. }, Some(e)) => {
                // The mirror took the new name when the update was sent: restore it from the
                // device.
                self.rpc_err(out, &p, e);
                self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
            }
            (_, Some(e)) => self.rpc_err(out, &p, e),
        }
        if p.truncated {
            out.push(Output::Event(Event::Log(format!(
                "management reply {} arrived truncated (chunk gap)",
                p.req_id
            ))));
        }
    }

    // ---- MQTT → engine ----------------------------------------------------------

    fn on_mqtt(&mut self, topic: &str, data: &[u8], retain: bool, out: &mut Vec<Output>) {
        let inbound = topics::classify(&self.prefix, topic);
        if inbound == topics::Inbound::Other || data.is_empty() {
            return; // not ours / an empty publish (e.g. a retained-command clear echoing back)
        }
        if retain {
            // A *stored* retained message, replayed by the broker on (re)subscribe: run it
            // and it re-runs on every reconnect, forever (a retained `node_remove`, a
            // retained shell line). Live deliveries carry retain=0, so a retained publish
            // already ran once when it was sent: refuse the replay and clear the stored copy.
            out.push(Output::Event(Event::Log(format!(
                "ignoring a retained command on {topic} (commands must not be retained); clearing it"
            ))));
            out.push(Output::Publish {
                topic: topic.to_string(),
                payload: Vec::new(),
                retain: true,
            });
            return;
        }
        if data.len() > MAX_COMMAND_PAYLOAD {
            out.push(Output::Event(Event::Log(format!(
                "ignoring an oversized command on {topic} ({} B > {MAX_COMMAND_PAYLOAD} B)",
                data.len()
            ))));
            return;
        }
        match inbound {
            topics::Inbound::Cmd => match serde_json::from_slice::<RpcRequest>(data) {
                Ok(req) => {
                    if self.accept_request_id(&req.id, topic, out) {
                        self.on_rpc(req, RpcOrigin::Mqtt, out);
                    }
                }
                Err(e) => out.push(Output::Event(Event::Log(format!("bad RPC payload: {e}")))),
            },
            topics::Inbound::ShellReq(node) => match serde_json::from_slice::<payload::ShellReq>(data) {
                Ok(req) => {
                    if self.accept_request_id(&req.id, topic, out) {
                        self.on_shell_req(node, req, out);
                    }
                }
                Err(e) => out.push(Output::Event(Event::Log(format!("bad shell request: {e}")))),
            },
            topics::Inbound::Other => {}
        }
    }

    /// Gate an MQTT request id. A malformed one is refused (it would be echoed into a
    /// response topic and payload; see [`payload::valid_request_id`]; with no safe topic
    /// to answer on, the request is dropped with a log line). A QoS-1 *redelivery* of an id
    /// already handled is dropped too: at-least-once would otherwise run a shell command,
    /// a removal or a pairing window twice.
    fn accept_request_id(&mut self, id: &str, topic: &str, out: &mut Vec<Output>) -> bool {
        if !payload::valid_request_id(id) {
            out.push(Output::Event(Event::Log(format!(
                "ignoring a request on {topic}: invalid id (want 1..={} chars of [A-Za-z0-9_.-])",
                payload::MAX_REQUEST_ID
            ))));
            return false;
        }
        if self.recent_ids.iter().any(|r| r == id) {
            out.push(Output::Event(Event::Log(format!(
                "ignoring duplicate request {id} on {topic} (MQTT redelivery)"
            ))));
            return false;
        }
        if self.recent_ids.len() >= RECENT_IDS {
            self.recent_ids.pop_front();
        }
        self.recent_ids.push_back(id.to_string());
        true
    }

    /// Validate a node name for `node_add` / `node_rename`: the host is the only writer
    /// (the gateway firmware stores whatever it is given): 1..=`MAX_NODE_NAME` bytes of
    /// printable text ([`printable_name`]), not a node-address lookalike (`<node>`
    /// arguments resolve hex first, so such a name could never be addressed), and unique:
    /// name lookup takes the first match, so a duplicate would silently shadow (and
    /// misroute commands meant for) one of the two.
    fn check_name(&self, name: &str, addr: u32) -> Result<(), String> {
        if name.is_empty() || name.len() > mgmt::MAX_NODE_NAME {
            return Err(format!("name must be 1..={} bytes", mgmt::MAX_NODE_NAME));
        }
        if !printable_name(name) {
            return Err("name must be printable text (no control characters)".into());
        }
        if topics::parse_node_hex(name).is_some() {
            return Err("name must not look like a node address (0xHHHHHHHH)".into());
        }
        if let Some(other) = self
            .nodes
            .iter()
            .find(|n| n.entry.addr != addr && n.entry.name == name)
        {
            return Err(format!(
                "name \"{name}\" is already used by {}",
                topics::node_hex(other.entry.addr)
            ));
        }
        Ok(())
    }

    /// Enqueue one remote-shell line for `node` (from `nodes/{id}/shell/req`).
    pub(crate) fn on_shell_req(&mut self, node: u32, req: payload::ShellReq, out: &mut Vec<Output>) {
        let fail = |engine: &Engine, out: &mut Vec<Output>, error: &str| {
            let rsp = payload::ShellRsp {
                id: req.id.clone(),
                chunk: 0,
                text: String::new(),
                done: true,
                result: 1,
                truncated: false,
                error: Some(error.to_string()),
            };
            out.push(Output::Publish {
                topic: topics::node_shell_rsp(&engine.prefix, node),
                payload: json(&rsp),
                retain: false,
            });
            out.push(Output::Event(Event::Shell { node, rsp }));
        };
        if self.nodes.iter().all(|n| n.entry.addr != node) {
            return fail(self, out, "unknown node");
        }
        if req.line.len() > radio::RADIO_SHELL_CHUNK {
            return fail(
                self,
                out,
                &format!(
                    "line too long for the radio MTU (max {} bytes)",
                    radio::RADIO_SHELL_CHUNK
                ),
            );
        }
        let cmd_id = self.next_cmd;
        self.next_cmd = self.next_cmd.checked_add(1).filter(|&v| v != 0).unwrap_or(1);
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let Ok(n) = radio::encode_node_cmd(
            &NodeCmd::Shell {
                epoch: self.shell_epoch,
                cmd_id,
                line: &req.line,
            },
            &mut env,
        ) else {
            return fail(self, out, "envelope encode failed");
        };
        let data = env[..n].to_vec();
        let origin = ShellOrigin {
            rpc: req.id.clone(),
            line: req.line.clone(),
            cmd_id,
            ttl: req.ttl,
        };
        // The borrow-carrying op is encoded immediately by `issue`. The enqueue carries no
        // RPC id: its failure (refusal, timeout, link down) answers on `shell/rsp` instead
        // (see `fail_op`).
        let op = MgmtOp::QueuePush {
            node_addr: node,
            ttl: req.ttl,
            data: &data,
        };
        self.issue(
            out,
            OpKind::QueuePush {
                node,
                shell: Some(origin),
            },
            op,
            None,
        );
    }

    /// The `gateway/cmd` RPC surface: what the `tower nodes`/`net` client commands
    /// (and the TUI, in-process) drive. `origin` decides where the response goes.
    fn on_rpc(&mut self, req: RpcRequest, origin: RpcOrigin, out: &mut Vec<Output>) {
        let me = RpcRef {
            id: req.id.clone(),
            origin,
        };
        let rpc = Some(me.clone());
        let params = &req.params;
        let node_param = || -> Option<u32> {
            let v = params.get("addr")?.as_str()?;
            topics::parse_node_hex(v).or_else(|| {
                self.nodes
                    .iter()
                    .find(|n| n.entry.name == v)
                    .map(|n| n.entry.addr)
            })
        };
        match req.op.as_str() {
            "describe" => {
                self.issue(out, OpKind::Describe, MgmtOp::Describe, rpc);
            }
            "node_list" => {
                self.issue(out, OpKind::NodeList, MgmtOp::NodeList, rpc);
            }
            "node_add" => {
                // Cable pairing: the client provisioned the node on its own serial
                // port and registers (id, key, name) here, with a freshly minted key (no
                // floors). A key moved from another gateway (a `reveal_key` there) must bring
                // that answer's `tx_floor` / `rx_floor` along, or this gateway may repeat the
                // key's old nonces.
                let id = params
                    .get("addr")
                    .and_then(|v| v.as_str())
                    .and_then(topics::parse_node_hex);
                let key = params.get("key").and_then(|v| v.as_str()).and_then(parse_key_hex);
                let floor = |name: &str| match params.get(name) {
                    None | Some(serde_json::Value::Null) => Some(0),
                    Some(v) => v.as_u64().and_then(|f| u32::try_from(f).ok()),
                };
                let (Some(tx_floor), Some(rx_floor)) = (floor("tx_floor"), floor("rx_floor")) else {
                    return self.rpc_err_direct(
                        out,
                        &me,
                        "node_add: tx_floor / rx_floor must be u32 numbers",
                    );
                };
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let sleeping = params.get("sleeping").and_then(|v| v.as_bool()).unwrap_or(true);
                let (Some(id), Some(key)) = (id, key) else {
                    return self.rpc_err_direct(out, &me, "node_add needs addr + key (hex)");
                };
                if !name.is_empty()
                    && let Err(e) = self.check_name(&name, id)
                {
                    return self.rpc_err_direct(out, &me, &e);
                }
                let mut flags = if sleeping { mgmt::NODE_FLAG_SLEEPING } else { 0 };
                if name.is_empty() {
                    flags |= mgmt::NODE_FLAG_UNNAMED;
                }
                let op = MgmtOp::NodeAdd {
                    addr: id,
                    key,
                    name: name.as_str(),
                    flags,
                    tx_floor,
                    rx_floor,
                };
                self.issue(
                    out,
                    OpKind::NodeAdd {
                        id,
                        name: name.clone(),
                    },
                    op,
                    rpc,
                );
            }
            "node_remove" => match node_param() {
                Some(id) => {
                    self.issue(
                        out,
                        OpKind::NodeRemove { id },
                        MgmtOp::NodeRemove { addr: id },
                        rpc,
                    );
                }
                None => self.rpc_err_direct(out, &me, "unknown node"),
            },
            "node_rename" => {
                // Only a mirrored node: the dongle's `NodeUpdate` replaces the flags, so the
                // rename carries the node's own with `unnamed` cleared (sent without flags, the
                // node stayed unnamed and its next `NodeInfo` auto-named over the rename), and
                // `check_name` needs the other names to refuse a duplicate. Before the startup
                // listing lands, every node is unknown here.
                let Some((id, flags)) = node_param().and_then(|id| {
                    self.nodes
                        .iter()
                        .find(|n| n.entry.addr == id)
                        .map(|n| (id, n.entry.flags & !mgmt::NODE_FLAG_UNNAMED))
                }) else {
                    return self.rpc_err_direct(out, &me, "unknown node");
                };
                let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                    return self.rpc_err_direct(out, &me, "missing name");
                };
                if let Err(e) = self.check_name(name, id) {
                    return self.rpc_err_direct(out, &me, &e);
                }
                let op = MgmtOp::NodeUpdate {
                    addr: id,
                    name: Some(name),
                    flags: Some(flags),
                };
                // Mirror the name now: the node's next `NodeInfo` may land before the gateway
                // answers, and an unnamed mirror would auto-name it over the operator's choice.
                // Only a queued rename, though: one refused up front never reaches the dongle.
                if self.issue(out, OpKind::NodeUpdate { id }, op, rpc).is_some()
                    && let Some(n) = self.nodes.iter_mut().find(|n| n.entry.addr == id)
                {
                    n.entry.name = name.to_string();
                    n.entry.flags = flags;
                }
            }
            "reveal_key" => match node_param() {
                Some(id) => {
                    self.issue(
                        out,
                        OpKind::RevealKey { id },
                        MgmtOp::NodeRevealKey { addr: id },
                        rpc,
                    );
                }
                None => self.rpc_err_direct(out, &me, "unknown node"),
            },
            "node_add_ota" => {
                if self.pairing_req.is_some() {
                    return self.rpc_err_direct(out, &me, "a pairing window is already open");
                }
                // A cancelled window stays open on the dongle until it takes the cancel and
                // answers the cancelled `PairingOpen`; a new one before that would be refused
                // `busy`, or open while the old answer is still on its way.
                if self
                    .pending_mgmt
                    .iter()
                    .any(|p| matches!(p.op, OpKind::PairingOpen))
                {
                    return self.rpc_err_direct(
                        out,
                        &me,
                        "the cancelled pairing window has not closed yet; try again",
                    );
                }
                let window = params
                    .get("window")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(60)
                    .clamp(1, 600) as u16;
                // No key to mint: OTA pairing derives the link key on both ends (X25519).
                // The window shows as open once its request is queued (the dongle answers only
                // when it ends, or at once with a refusal, which closes it again).
                if let Some(req_id) = self.issue_delayed(
                    out,
                    OpKind::PairingOpen,
                    MgmtOp::PairingOpen { window },
                    rpc,
                    window as u64 + PAIRING_SLACK_S,
                ) {
                    self.pairing_req = Some(req_id);
                    self.pairing_until = Some(self.tick + window as u64);
                    self.publish_pairing(out, None, None);
                }
            }
            "pairing_cancel" => {
                self.pairing_req = None;
                self.pairing_until = None;
                self.publish_pairing(out, None, None);
                self.issue(out, OpKind::PairingCancel, MgmtOp::PairingCancel, rpc);
            }
            "queue_list" => {
                // No `addr`: every node's queue. One that names no node is an error, not "all".
                let node = match params.get("addr") {
                    None | Some(serde_json::Value::Null) => 0,
                    Some(_) => match node_param() {
                        Some(node) => node,
                        None => return self.rpc_err_direct(out, &me, "unknown node"),
                    },
                };
                self.issue(out, OpKind::QueueList, MgmtOp::QueueList { node_addr: node }, rpc);
            }
            "queue_drop" => {
                let Some(node) = node_param() else {
                    return self.rpc_err_direct(out, &me, "unknown node");
                };
                // `ref` absent = the node's whole queue. A present one must be a real item
                // id: `as u16` used to truncate 65536+ into some *other* live item.
                let item = match params.get("ref") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(v) => match v.as_u64().and_then(|v| u16::try_from(v).ok()).filter(|&v| v != 0) {
                        Some(it) => Some(it),
                        None => {
                            return self.rpc_err_direct(out, &me, "ref must be a queue item id (1..=65535)");
                        }
                    },
                };
                // Prune the mirror now (the device answer confirms; a NotFound still
                // clears our side, which matches "it isn't queued anymore"). Without a
                // ref the device drops the node's WHOLE queue, so the mirror drops every
                // undelivered entry too: no phantom pendings. Delivered entries are no
                // longer on the device; they stay, awaiting the node's reply.
                if let Some(i) = self.nodes.iter().position(|n| n.entry.addr == node) {
                    let (gone, keep): (Vec<PendingShell>, Vec<PendingShell>) =
                        std::mem::take(&mut self.nodes[i].pending)
                            .into_iter()
                            .partition(|pd| match item {
                                Some(it) => pd.item == it,
                                None => pd.delivered_tick.is_none(),
                            });
                    self.nodes[i].pending = keep;
                    if !gone.is_empty() {
                        for pd in &gone {
                            self.shell_error(out, node, pd, "dequeued");
                        }
                        self.publish_pending(out, i);
                    }
                }
                self.issue(
                    out,
                    OpKind::QueueDrop { node },
                    MgmtOp::QueueDrop {
                        node_addr: node,
                        item,
                    },
                    rpc,
                );
            }
            "stats_config" => {
                // Absent: the firmware's own `stats-period` default (500 ms).
                let period = params
                    .get("period_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(500)
                    .min(60_000) as u32;
                self.issue(
                    out,
                    OpKind::StatsConfig,
                    MgmtOp::StatsConfig {
                        channel_period_ms: period,
                    },
                    rpc,
                );
            }
            other => self.rpc_err_direct(out, &me, &format!("unknown op \"{other}\"")),
        }
    }

    // ---- housekeeping -------------------------------------------------------------

    fn on_tick(&mut self, out: &mut Vec<Output>) {
        self.tick += 1;
        // Management timeouts.
        let expired: Vec<usize> = self
            .pending_mgmt
            .iter()
            .enumerate()
            .filter(|(_, p)| self.tick >= p.deadline)
            .map(|(i, _)| i)
            .collect();
        let mut resync_queue = false;
        for i in expired.into_iter().rev() {
            let p = self.pending_mgmt.remove(i);
            out.push(Output::Event(Event::Log(format!(
                "management op timed out (req {})",
                p.req_id
            ))));
            // An unanswered enqueue may still have landed on the device (only the reply
            // was lost): reconcile afterwards so such an orphan is dropped, not run with
            // nobody listening, which keeps the "not queued" answer below true. An unanswered
            // drop may never have landed: its dialogs already ended `dequeued`, so the item
            // left queued is just such an orphan.
            resync_queue |= matches!(p.op, OpKind::QueuePush { .. } | OpKind::QueueDrop { .. });
            if matches!(p.op, OpKind::PairingOpen) {
                // The window's answer may still come: a late `Paired` is a node the gateway did
                // register, so it is reported when it lands (`on_late_pairing`), not dropped.
                self.late_pairing = Some(p.req_id);
            }
            self.fail_op(
                out,
                p.op,
                Some(p.req_id),
                p.rpc.as_ref(),
                "device did not answer (timeout)",
            );
        }
        if resync_queue {
            self.issue(out, OpKind::QueueSync, MgmtOp::QueueList { node_addr: 0 }, None);
        }
        // A timed-out request, or a `PairingOpen` past its pickup time, frees the slot.
        self.send_next_mgmt(out);
        // The startup registry sync must land eventually: uplinks are held until it does,
        // so a refused/timed-out first NodeList is retried rather than leaving them parked.
        if !self.registry_synced
            && self.serial_up
            && !self.pending_mgmt.iter().any(|p| matches!(p.op, OpKind::NodeList))
        {
            self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
        }
        // Remote-shell timeouts: delivered but never answered; or never delivered and
        // past its queue TTL with no `TX_EXPIRED` report (lost to a missed gateway reboot
        // or link drop), so no pending entry can outlive its TTL.
        for i in 0..self.nodes.len() {
            let node = self.nodes[i].entry.addr;
            let tick = self.tick;
            let (dead, keep): (Vec<PendingShell>, Vec<PendingShell>) =
                std::mem::take(&mut self.nodes[i].pending)
                    .into_iter()
                    .partition(|p| match p.delivered_tick {
                        Some(t) => tick >= t + SHELL_REPLY_TIMEOUT_TICKS,
                        None => tick >= p.expires_tick,
                    });
            self.nodes[i].pending = keep;
            if dead.is_empty() {
                continue;
            }
            for p in &dead {
                let why = if p.delivered_tick.is_some() {
                    "delivered but no reply from the node"
                } else {
                    "queue TTL elapsed with no delivery report from the gateway"
                };
                self.shell_error(out, node, p, why);
            }
            self.publish_pending(out, i);
        }
        // Pairing countdown surface.
        if self.pairing_until.is_some() {
            self.publish_pairing(out, None, None);
        }
        if self.tick.is_multiple_of(STATS_EVERY_TICKS) {
            self.publish_stats(out);
        }
    }

    // ---- helpers --------------------------------------------------------------------

    /// Allocate a req_id, encode and send `op`, and track the reply. Returns the req_id, or
    /// `None` when the op was failed up front (link down, encode failure) and will never reach
    /// the dongle: a caller that mirrors the op's effect does so only on `Some`.
    fn issue(
        &mut self,
        out: &mut Vec<Output>,
        kind: OpKind,
        op: MgmtOp<'_>,
        rpc: Option<RpcRef>,
    ) -> Option<u16> {
        self.issue_delayed(out, kind, op, rpc, MGMT_TIMEOUT_TICKS)
    }

    fn issue_delayed(
        &mut self,
        out: &mut Vec<Output>,
        kind: OpKind,
        op: MgmtOp<'_>,
        rpc: Option<RpcRef>,
        timeout_ticks: u64,
    ) -> Option<u16> {
        if !self.serial_up {
            // The serial thread drops frames while the port is down: fail now instead of
            // after a timeout (or, for a shell enqueue, never). Engine-internal ops need no
            // answer: the reconnect resync re-issues what matters.
            self.fail_op(out, kind, None, rpc.as_ref(), "gateway serial link is down");
            return None;
        }
        let req_id = self.next_req;
        self.next_req = self.next_req.checked_add(1).filter(|&v| v != 0).unwrap_or(1);
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        match encode_mgmt(seq, req_id, &op) {
            Some(frame) => {
                self.pending_mgmt.push(PendingMgmt {
                    req_id,
                    op: kind,
                    rpc,
                    data: Vec::new(),
                    next_chunk: 0,
                    truncated: false,
                    deadline: u64::MAX,
                    frame: Some(frame),
                    timeout_ticks,
                    holds_slot_until: u64::MAX,
                });
                self.send_next_mgmt(out);
                Some(req_id)
            }
            None => {
                self.fail_op(out, kind, Some(req_id), rpc.as_ref(), "request encode failed");
                None
            }
        }
    }

    /// Send the oldest waiting request once the dongle's request slot is free. The gateway takes
    /// requests through a one-deep inbox: two sent back to back lose the second (it used to be
    /// the startup queue sync, every time), so one request is out at a time and the rest wait
    /// here in issue order ([`PAIRING_PICKUP_TICKS`] for the one op answered late).
    fn send_next_mgmt(&mut self, out: &mut Vec<Output>) {
        let tick = self.tick;
        if self
            .pending_mgmt
            .iter()
            .any(|p| p.frame.is_none() && tick < p.holds_slot_until)
        {
            return;
        }
        let Some(p) = self.pending_mgmt.iter_mut().find(|p| p.frame.is_some()) else {
            return;
        };
        let frame = p.frame.take().expect("found by frame");
        p.deadline = tick + p.timeout_ticks;
        if matches!(p.op, OpKind::PairingOpen) {
            p.holds_slot_until = tick + PAIRING_PICKUP_TICKS;
        }
        out.push(Output::SerialSend(frame));
    }

    /// The serial link dropped: requests still waiting for the slot were never sent and won't be
    /// (the serial thread drops frames while the port is down), so fail them now. Sent ones wait
    /// out their deadline; the reconnect resync re-issues what the engine needs.
    fn fail_unsent_mgmt(&mut self, out: &mut Vec<Output>) {
        let (unsent, sent): (Vec<PendingMgmt>, Vec<PendingMgmt>) = std::mem::take(&mut self.pending_mgmt)
            .into_iter()
            .partition(|p| p.frame.is_some());
        self.pending_mgmt = sent;
        for p in unsent {
            self.fail_op(
                out,
                p.op,
                Some(p.req_id),
                p.rpc.as_ref(),
                "gateway serial link is down",
            );
        }
    }

    /// An op that will never be answered (timeout, serial down, encode failure): tell
    /// whoever is waiting. An RPC gets its error response; a shell enqueue (which has no
    /// RPC id) gets a terminal `shell/rsp` error (it used to vanish silently while the
    /// client waited out its timeout); a pairing window resets the published state.
    /// `req_id` is `None` for an op failed before it was tracked.
    fn fail_op(
        &mut self,
        out: &mut Vec<Output>,
        op: OpKind,
        req_id: Option<u16>,
        rpc: Option<&RpcRef>,
        error: &str,
    ) {
        match op {
            OpKind::PairingOpen => {
                if let Some(req_id) = req_id {
                    self.end_pairing(out, req_id);
                }
            }
            OpKind::QueuePush {
                node,
                shell: Some(origin),
            } => {
                let rsp = payload::ShellRsp {
                    id: origin.rpc,
                    chunk: 0,
                    text: String::new(),
                    done: true,
                    result: 1,
                    truncated: false,
                    error: Some(format!("not queued: {error}")),
                };
                out.push(Output::Publish {
                    topic: topics::node_shell_rsp(&self.prefix, node),
                    payload: json(&rsp),
                    retain: false,
                });
                out.push(Output::Event(Event::Shell { node, rsp }));
            }
            // The mirror took the new name when the update was queued: restore it from the
            // device (with the link down, the reconnect's listing does).
            OpKind::NodeUpdate { .. } if self.serial_up => {
                self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
            }
            _ => {}
        }
        if let Some(r) = rpc {
            self.rpc_err_direct(out, r, error);
        }
    }

    fn rpc_ok(&self, out: &mut Vec<Output>, p: &PendingMgmt, data: serde_json::Value) {
        if let Some(r) = &p.rpc {
            self.rpc_respond(
                out,
                r.origin,
                RpcResponse {
                    id: r.id.clone(),
                    ok: true,
                    error: None,
                    data,
                },
            );
        }
    }

    fn rpc_err(&self, out: &mut Vec<Output>, p: &PendingMgmt, error: &str) {
        if let Some(r) = &p.rpc {
            self.rpc_err_direct(out, r, error);
        }
    }

    fn rpc_err_direct(&self, out: &mut Vec<Output>, rpc: &RpcRef, error: &str) {
        self.rpc_respond(
            out,
            rpc.origin,
            RpcResponse {
                id: rpc.id.clone(),
                ok: false,
                error: Some(error.to_string()),
                data: serde_json::Value::Null,
            },
        );
    }

    /// Route an RPC response by the origin carried since its request came in: the
    /// in-process frontend gets an event, MQTT clients the response topic. (It used to
    /// key off a `tui-` id prefix, so any MQTT client could steer responses into the TUI
    /// or the cable-pairing worker, e.g. answer its predictable `tui-cbl-N` itself.)
    fn rpc_respond(&self, out: &mut Vec<Output>, origin: RpcOrigin, rsp: RpcResponse) {
        match origin {
            RpcOrigin::Frontend => out.push(Output::Event(Event::Rpc(rsp))),
            RpcOrigin::Mqtt => out.push(Output::Publish {
                topic: topics::gateway_rsp(&self.prefix, &rsp.id),
                payload: json(&rsp),
                retain: false,
            }),
        }
    }

    /// Publish a terminal error for a pending shell command (dialog + rsp topic).
    fn shell_error(&self, out: &mut Vec<Output>, node: u32, p: &PendingShell, error: &str) {
        let rsp = payload::ShellRsp {
            id: p.rpc.clone(),
            chunk: p.next_chunk,
            text: String::new(),
            done: true,
            result: 1,
            truncated: p.truncated,
            error: Some(error.to_string()),
        };
        out.push(Output::Publish {
            topic: topics::node_shell_rsp(&self.prefix, node),
            payload: json(&rsp),
            retain: false,
        });
        out.push(Output::Event(Event::Shell { node, rsp }));
    }

    /// Mirror a node the gateway just confirmed (OTA join / cable add) ahead of the
    /// `NodeList` refresh, so its first uplinks (typically the `NodeInfo` that drives
    /// auto-naming) aren't dropped as unregistered in the window before the list answers.
    /// The refresh's merge then replaces the row with device truth.
    fn provisional_node(&mut self, addr: u32, name: &str, flags: u8) {
        if self.nodes.iter().any(|n| n.entry.addr == addr) {
            return;
        }
        self.nodes.push(NodeMirror {
            entry: NodeEntryOwned {
                addr,
                name: name.to_string(),
                flags,
                last_seen: mgmt::LAST_SEEN_NEVER,
                rssi: mgmt::RSSI_NONE,
                uplinks: 0,
                queued: 0,
            },
            kind: String::new(),
            session_id: None,
            pending: Vec::new(),
            last_counter: None,
        });
    }

    /// Reconcile the pending-shell mirror with the gateway's real queue (a `QueueSync`
    /// listing of `(node, item)`):
    ///
    /// - an **undelivered** mirror entry the gateway no longer holds was lost: its request
    ///   never reached the device, or it was delivered/dropped while the host link was
    ///   down and the report (and any reply) went with it → fail it loudly;
    /// - a device item the mirror doesn't know is a leftover (a previous `tower gateway`
    ///   process, or an enqueue whose reply was lost) whose reply could only be
    ///   mis-credited → drop it on the device.
    ///
    /// Delivered entries are legitimately gone from the device (awaiting the node's reply).
    fn reconcile_queue(&mut self, device: &[(u32, u16)], out: &mut Vec<Output>) {
        for i in 0..self.nodes.len() {
            let node = self.nodes[i].entry.addr;
            let (lost, keep): (Vec<PendingShell>, Vec<PendingShell>) =
                std::mem::take(&mut self.nodes[i].pending)
                    .into_iter()
                    .partition(|p| p.delivered_tick.is_none() && !device.contains(&(node, p.item)));
            self.nodes[i].pending = keep;
            if !lost.is_empty() {
                for p in &lost {
                    self.shell_error(
                        out,
                        node,
                        p,
                        "no longer queued on the gateway (lost while the host link was down)",
                    );
                }
                self.publish_pending(out, i);
            }
        }
        let stale: Vec<(u32, u16)> = device
            .iter()
            .copied()
            .filter(|&(node, item)| {
                !self
                    .nodes
                    .iter()
                    .any(|n| n.entry.addr == node && n.pending.iter().any(|p| p.item == item))
            })
            .collect();
        if stale.is_empty() {
            return;
        }
        out.push(Output::Event(Event::Log(format!(
            "dropping {} stale queued downlink(s) this host has no record of (left by a previous `tower gateway`?)",
            stale.len()
        ))));
        for (node, item) in stale {
            self.issue(
                out,
                OpKind::QueueDropStale,
                MgmtOp::QueueDrop {
                    node_addr: node,
                    item: Some(item),
                },
                None,
            );
        }
        // Refresh the per-node `queued` counts (answered after the drops, in order).
        self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
    }

    /// Node `id` left the registry, seen in the `NodeRemove` answer or in a listing without it:
    /// end each of its pending shell dialogs (`pending`, taken out of its dropped mirror row)
    /// with its one terminal `done`, and clear its retained topics (empty payloads).
    fn node_gone(&self, out: &mut Vec<Output>, id: u32, pending: &[PendingShell]) {
        for pd in pending {
            self.shell_error(out, id, pd, "node removed");
        }
        for topic in [
            topics::node(&self.prefix, id),
            topics::node_temperature(&self.prefix, id),
            topics::node_shell_pending(&self.prefix, id),
        ] {
            out.push(Output::Publish {
                topic,
                payload: Vec::new(),
                retain: true,
            });
        }
    }

    /// Merge a fresh `NodeList` into the mirror: keep host-side enrichment (kind,
    /// session, pendings) for surviving nodes, publish adds/changes, clear removals.
    fn merge_registry(&mut self, fresh: Vec<NodeEntryOwned>, out: &mut Vec<Output>) {
        // A rename or flags change still waiting for its answer was mirrored when it was issued,
        // and a listing answered before it predates it: keep the mirror's name and flags, or the
        // node's next `NodeInfo` would see it unnamed again and auto-name over the rename.
        let updating: Vec<u32> = self
            .pending_mgmt
            .iter()
            .filter_map(|p| match p.op {
                OpKind::NodeUpdate { id } => Some(id),
                _ => None,
            })
            .collect();
        let mut next: Vec<NodeMirror> = Vec::with_capacity(fresh.len());
        for mut entry in fresh {
            let old = self.nodes.iter_mut().find(|n| n.entry.addr == entry.addr);
            match old {
                Some(o) => next.push(NodeMirror {
                    entry: {
                        if updating.contains(&entry.addr) {
                            entry.name = std::mem::take(&mut o.entry.name);
                            entry.flags = o.entry.flags;
                        }
                        entry
                    },
                    kind: o.kind.clone(),
                    session_id: o.session_id,
                    pending: std::mem::take(&mut o.pending),
                    last_counter: o.last_counter,
                }),
                None => next.push(NodeMirror {
                    entry,
                    kind: String::new(),
                    session_id: None,
                    pending: Vec::new(),
                    last_counter: None,
                }),
            }
        }
        // Anything left in the old mirror was removed on the device (e.g. a `node_remove` whose
        // answer was lost): clean up as that answer would have.
        let old = std::mem::replace(&mut self.nodes, next);
        for gone in old
            .iter()
            .filter(|o| self.nodes.iter().all(|n| n.entry.addr != o.entry.addr))
        {
            self.node_gone(out, gone.entry.addr, &gone.pending);
        }
        for i in 0..self.nodes.len() {
            self.publish_node(out, i);
        }
        self.emit_registry(out);
    }

    /// Refresh a node's RAM-side liveliness on an uplink (the device tracks this too;
    /// mirroring it keeps the TUI live without polling NodeList).
    fn touch_node(&mut self, id: u32, rssi: i16) {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.entry.addr == id) {
            n.entry.last_seen = 0;
            n.entry.rssi = rssi.clamp(i8::MIN as i16, i8::MAX as i16) as i8;
            n.entry.uplinks = n.entry.uplinks.saturating_add(1);
        }
    }

    fn node_payloads(&self) -> Vec<payload::Node> {
        self.nodes
            .iter()
            .map(|n| payload::Node {
                addr: topics::node_hex(n.entry.addr),
                name: n.entry.name.clone(),
                kind: n.kind.clone(),
                sleeping: n.entry.flags & mgmt::NODE_FLAG_SLEEPING != 0,
                unnamed: n.entry.flags & mgmt::NODE_FLAG_UNNAMED != 0,
                last_seen: (n.entry.last_seen != mgmt::LAST_SEEN_NEVER).then_some(n.entry.last_seen),
                rssi: (n.entry.rssi != mgmt::RSSI_NONE).then_some(n.entry.rssi),
                uplinks: n.entry.uplinks,
                queued: n.entry.queued.max(n.pending.len() as u8),
            })
            .collect()
    }

    fn publish_node(&self, out: &mut Vec<Output>, i: usize) {
        let payloads = self.node_payloads();
        out.push(Output::Publish {
            topic: topics::node(&self.prefix, self.nodes[i].entry.addr),
            payload: json(&payloads[i]),
            retain: true,
        });
    }

    fn publish_pending(&self, out: &mut Vec<Output>, i: usize) {
        let entries: Vec<PendingEntry> = self.nodes[i]
            .pending
            .iter()
            .map(|p| PendingEntry {
                r#ref: p.item,
                id: p.rpc.clone(),
                line: p.line.clone(),
            })
            .collect();
        out.push(Output::Publish {
            topic: topics::node_shell_pending(&self.prefix, self.nodes[i].entry.addr),
            payload: json(&entries),
            retain: true,
        });
        self.emit_registry(out);
    }

    fn emit_registry(&self, out: &mut Vec<Output>) {
        let views: Vec<NodeView> = self
            .nodes
            .iter()
            .map(|n| NodeView {
                addr: n.entry.addr,
                name: n.entry.name.clone(),
                kind: n.kind.clone(),
                sleeping: n.entry.flags & mgmt::NODE_FLAG_SLEEPING != 0,
                unnamed: n.entry.flags & mgmt::NODE_FLAG_UNNAMED != 0,
                last_seen: (n.entry.last_seen != mgmt::LAST_SEEN_NEVER).then_some(n.entry.last_seen),
                rssi: (n.entry.rssi != mgmt::RSSI_NONE).then_some(n.entry.rssi),
                uplinks: n.entry.uplinks,
                queued: n.entry.queued.max(n.pending.len() as u8),
                pending: n
                    .pending
                    .iter()
                    .map(|p| PendingEntry {
                        r#ref: p.item,
                        id: p.rpc.clone(),
                        line: p.line.clone(),
                    })
                    .collect(),
            })
            .collect();
        out.push(Output::Event(Event::Registry(views)));
    }

    fn publish_status(&self, out: &mut Vec<Output>) {
        let state = if self.serial_up { "online" } else { "degraded" };
        out.push(Output::Publish {
            topic: topics::gateway_status(&self.prefix),
            payload: json(&payload::Status {
                state: state.into(),
                schema: payload::SCHEMA,
                gateway: topics::node_hex(self.describe.addr),
                firmware: self.describe.firmware_name.clone(),
                // From the startup `Describe` (`DeviceInfo.firmware_version`), then refreshed by
                // every Hello / Describe: known even if no boot Hello was seen.
                firmware_version: self.describe.firmware_version.clone(),
                session_id: self.hello_session.unwrap_or(0),
                wire: payload::wire_tags(),
                serial_port: self.port_name.clone(),
                serial_up: self.serial_up,
            }),
            retain: true,
        });
    }

    fn publish_stats(&self, out: &mut Vec<Output>) {
        out.push(Output::Publish {
            topic: topics::gateway_stats(&self.prefix),
            payload: json(&payload::Stats {
                uptime: self.tick,
                nodes: self.nodes.len() as u32,
                uplinks: self.uplinks,
                queued: self.nodes.iter().map(|n| n.pending.len() as u32).sum(),
                rssi: self.last_ambient.map(|(d, _)| d),
                channel: self.last_ambient.map(|(_, c)| c),
            }),
            retain: true,
        });
    }

    /// A node joined (the `PairingOpen` answer, or a `late` one that arrived after this side gave
    /// up on the window): mirror it, log and publish it with the handshake's SAS, refresh the
    /// registry. Returns the node's address and the SAS as the RPC shows them.
    fn on_paired(&mut self, j: Paired, late: bool, out: &mut Vec<Output>) -> (String, String) {
        self.provisional_node(j.addr, "", mgmt::NODE_FLAG_UNNAMED);
        let hex = topics::node_hex(j.addr);
        let sas = payload::sas_str(j.sas);
        out.push(Output::Event(Event::Log(if late {
            format!(
                "node {hex} paired after the window's answer timed out (SAS {sas}): it is registered; \
                 compare the SAS with the node's `net: pairing: SAS` log line, or remove it"
            )
        } else {
            format!("node {hex} paired (SAS {sas}): compare it with the node's `net: pairing: SAS` log line")
        })));
        self.publish_pairing(out, Some(hex.clone()), Some(sas.clone()));
        self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
        (hex, sas)
    }

    /// The answer to a `PairingOpen` that already timed out here (`late_pairing`). A `Paired`
    /// means the gateway registered a node after all (its window and commit ran past this side's
    /// slack): report it like a join, since dropping it would leave a registered node nobody was
    /// told about. Anything else (the window's expiry or a refusal) only confirms the timeout.
    fn on_late_pairing(&mut self, result: u8, data: &[u8], out: &mut Vec<Output>) {
        if result != mgmt::MGMT_OK {
            return;
        }
        match postcard::from_bytes::<Paired>(data) {
            Ok(j) => {
                let _ = self.on_paired(j, true, out);
            }
            Err(_) => out.push(Output::Event(Event::Log(
                "late pairing answer from the gateway did not decode; run `nodes list`".into(),
            ))),
        }
    }

    /// The `PairingOpen` `req_id` ended unjoined (expired, refused, failed): back to `idle`, but
    /// only if its window is the one shown (`pairing_req`); after a `pairing_cancel` it no
    /// longer is, and its answer must not close a window opened since.
    fn end_pairing(&mut self, out: &mut Vec<Output>, req_id: u16) {
        if self.pairing_req == Some(req_id) {
            self.pairing_req = None;
            self.pairing_until = None;
            self.publish_pairing(out, None, None);
        }
    }

    fn publish_pairing(&self, out: &mut Vec<Output>, joined: Option<String>, sas: Option<String>) {
        let open = self.pairing_until.is_some();
        let p = payload::Pairing {
            state: if open { "open" } else { "idle" }.into(),
            remaining: self.pairing_until.map(|u| u.saturating_sub(self.tick)),
            joined,
            sas,
        };
        out.push(Output::Publish {
            topic: topics::gateway_pairing(&self.prefix),
            payload: json(&p),
            retain: true,
        });
        out.push(Output::Event(Event::Pairing(p)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_protocol::mgmt::{DeviceRole, MGMT_OK, NODE_FLAG_SLEEPING, NODE_FLAG_UNNAMED};
    use tower_protocol::{MsgType, decode_frame};

    #[test]
    fn randomize_shell_ids_moves_off_the_defaults() {
        // Cross-restart dedup fix: production stamps a random 32-bit epoch on every
        // downlink shell command, so a fresh host reusing low cmd_ids is not mistaken
        // for a re-delivery the node already ran, and starts its cmd_ids at a random
        // point, so a late reply to the previous process's #1 isn't credited to ours.
        let mut e = engine();
        assert_eq!(e.shell_epoch, 0, "tests keep the deterministic epoch");
        assert_eq!(e.next_cmd, 1, "tests keep the deterministic first cmd_id");
        let (mut epoch_moved, mut cmd_moved) = (false, false);
        for _ in 0..4 {
            e.randomize_shell_ids();
            epoch_moved |= e.shell_epoch != 0;
            cmd_moved |= e.next_cmd != 1;
            assert_ne!(e.next_cmd, 0, "cmd_id 0 is never used");
        }
        assert!(epoch_moved, "a random epoch should differ from 0");
        assert!(cmd_moved, "a random first cmd_id should differ from 1");
    }

    fn engine() -> Engine {
        Engine::new(
            "tower/".into(),
            "/dev/ttyTEST".into(),
            crate::mgmt::DeviceInfoOwned {
                role: DeviceRole::Gateway,
                addr: 0x0000_0001,
                band: 0,
                channel: 0,
                node_capacity: 32,
                node_count: 0,
                provisioned: true,
                gw_addr: 0x0000_0001,
                firmware_name: "radio_dongle_gateway".into(),
                firmware_version: "v0.1.0".into(),
            },
        )
    }

    /// Decode the MgmtRequest inside an Output::SerialSend.
    fn sent_op(out: &Output) -> Option<(u16, String)> {
        let Output::SerialSend(frame) = out else {
            return None;
        };
        let mut dec = tower_protocol::FrameDecoder::new();
        let inner: Vec<u8> = frame.iter().find_map(|&b| dec.push(b).map(|s| s.to_vec()))?;
        let (mt, _, payload) = decode_frame(&inner).ok()?;
        if mt != MsgType::MgmtRequest {
            return None;
        }
        let req: tower_protocol::msg::MgmtRequest = postcard::from_bytes(payload).ok()?;
        Some((req.req_id, format!("{:?}", req.op)))
    }

    fn publishes<'a>(out: &'a [Output], topic: &str) -> Vec<&'a [u8]> {
        out.iter()
            .filter_map(|o| match o {
                Output::Publish {
                    topic: t, payload, ..
                } if t == topic => Some(payload.as_slice()),
                _ => None,
            })
            .collect()
    }

    /// Feed a complete single-chunk mgmt reply for the most recent request.
    fn reply(e: &mut Engine, req_id: u16, result: u8, data: &[u8]) -> Vec<Output> {
        e.handle(Input::SerialFrame(SerialMsg::Mgmt {
            req_id,
            result,
            chunk: 0,
            last: true,
            data: data.to_vec(),
        }))
    }

    /// The requests waiting for the dongle's request slot, decoded, oldest first.
    fn queued_ops(e: &Engine) -> Vec<String> {
        e.pending_mgmt
            .iter()
            .filter_map(|p| p.frame.clone())
            .filter_map(|f| sent_op(&Output::SerialSend(f)))
            .map(|(_, op)| op)
            .collect()
    }

    fn node_entry_record(id: u32, name: &str, flags: u8) -> Vec<u8> {
        postcard::to_stdvec(&NodeEntry {
            addr: id,
            name,
            flags,
            last_seen: 3,
            rssi: -60,
            uplinks: 5,
            queued: 0,
        })
        .unwrap()
    }

    /// Install one node into the engine mirror via a NodeList round-trip, then answer the
    /// startup queue sync (sent once the NodeList answer frees the request slot) with an empty
    /// queue, so the slot is free for the test's own requests.
    fn with_node(e: &mut Engine, id: u32, name: &str, flags: u8) {
        let start = e.start();
        let (req_id, op) = start.iter().find_map(sent_op).expect("start issues NodeList");
        assert!(op.contains("NodeList"));
        let out = reply(e, req_id, MGMT_OK, &node_entry_record(id, name, flags));
        assert!(!publishes(&out, &format!("tower/nodes/{}", topics::node_hex(id))).is_empty());
        let (sync, op) = out.iter().find_map(sent_op).expect("then the queue sync");
        assert!(op.contains("QueueList"), "{op}");
        let _ = reply(e, sync, MGMT_OK, &[]);
    }

    #[test]
    fn status_carries_the_firmware_version_from_describe() {
        // The startup Describe (`DeviceInfo.firmware_version`) seeds the version, so
        // the retained status (what `tower net status` prints) has it before, and without,
        // any boot Hello; the startup probe usually consumes that Hello anyway.
        let mut e = engine();
        let out = e.handle(Input::SerialUp);
        let raw = publishes(&out, &topics::gateway_status("tower/"));
        let st: payload::Status = serde_json::from_slice(raw[0]).unwrap();
        assert_eq!(st.firmware_version, "v0.1.0");
        assert_eq!(st.session_id, 0, "no Hello seen yet");
    }

    #[test]
    fn a_dongle_from_another_checkout_is_logged_as_a_schema_mismatch() {
        // A mid-session reflash to a firmware from another checkout: the serial thread maps its
        // frames to `Foreign` (never decoded); the engine says so, naming the firmware.
        let mut e = engine();
        let out = e.handle(Input::SerialFrame(SerialMsg::Foreign(Mismatch {
            tag: 0xBEEF,
            device: Some("radio_dongle_gateway v0.0.9".into()),
        })));
        assert!(out.iter().any(|o| matches!(
            o,
            Output::Event(Event::Log(l)) if l.contains("SCHEMA MISMATCH")
                && l.contains("0xbeef")
                && l.contains("radio_dongle_gateway v0.0.9")
                && l.contains("same checkout")
        )));
    }

    #[test]
    fn start_subscribes_and_lists() {
        let mut e = engine();
        let out = e.start();
        let subs: Vec<&String> = out
            .iter()
            .filter_map(|o| match o {
                Output::Subscribe(f) => Some(f),
                _ => None,
            })
            .collect();
        assert!(subs.iter().any(|f| *f == "tower/gateway/cmd"));
        assert!(subs.iter().any(|f| *f == "tower/nodes/+/shell/req"));
        assert!(out.iter().find_map(sent_op).is_some(), "a NodeList goes out");
    }

    #[test]
    fn node_list_reply_publishes_retained_registry() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", NODE_FLAG_SLEEPING);
        // The retained node payload carries the name and the sleeping flag.
        let out = reply_dummy_registry(&mut e);
        let raw = publishes(&out, "tower/nodes/0x0000ab12");
        assert!(!raw.is_empty());
        let n: payload::Node = serde_json::from_slice(raw[0]).unwrap();
        assert_eq!(n.name, "kitchen");
        assert!(n.sleeping);
    }

    /// Re-list helper so assertions read the publish of a fresh merge.
    fn reply_dummy_registry(e: &mut Engine) -> Vec<Output> {
        let mut out = Vec::new();
        e.issue(&mut out, OpKind::NodeList, MgmtOp::NodeList, None);
        let (req_id, _) = out.iter().find_map(sent_op).unwrap();
        reply(
            e,
            req_id,
            MGMT_OK,
            &node_entry_record(0xAB12, "kitchen", NODE_FLAG_SLEEPING),
        )
    }

    #[test]
    fn button_uplink_maps_to_topic() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Button {
                kind: radio::ButtonKind::Click,
                count: 7,
            },
            &mut env,
        )
        .unwrap();
        let out = e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src: 0xAB12,
            counter: 42,
            rssi: -67,
            lqi: 30,
            data: env[..n].to_vec(),
        }));
        let raw = publishes(&out, "tower/nodes/0x0000ab12/event/button");
        assert_eq!(raw.len(), 1);
        let ev: payload::ButtonEvent = serde_json::from_slice(raw[0]).unwrap();
        assert_eq!((ev.event.as_str(), ev.count, ev.counter), ("click", 7, 42));
        // The graph feed got an RX mark too.
        assert!(!publishes(&out, "tower/radio/rx").is_empty());
    }

    #[test]
    fn unnamed_node_is_auto_named_from_info() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "", NODE_FLAG_UNNAMED);
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Info(radio::NodeInfo {
                firmware_name: "radio_push_button",
                firmware_version: "v0.1.0",
                session_id: 1,
                sleeping: true,
            }),
            &mut env,
        )
        .unwrap();
        let out = e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src: 0xAB12,
            counter: 1,
            rssi: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        }));
        let op = out.iter().find_map(sent_op).expect("a NodeUpdate goes out");
        assert!(op.1.contains("NodeUpdate"), "{}", op.1);
        assert!(op.1.contains("push-button:0"), "auto-name in {}", op.1);
    }

    #[test]
    fn shell_req_roundtrip_queue_and_chunks() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", NODE_FLAG_SLEEPING);
        // 1. MQTT shell request → QueuePush.
        let out = e.handle(Input::MqttIn {
            topic: "tower/nodes/0x0000ab12/shell/req".into(),
            payload: serde_json::to_vec(&payload::ShellReq {
                id: "u-1".into(),
                line: "led on".into(),
                ttl: 0,
            })
            .unwrap(),
            retain: false,
        });
        let (req_id, op) = out.iter().find_map(sent_op).expect("QueuePush goes out");
        assert!(op.contains("QueuePush"), "{op}");
        // 2. QueueId reply → retained pending mirror.
        let qid = postcard::to_stdvec(&QueueId { item: 9 }).unwrap();
        let out = reply(&mut e, req_id, MGMT_OK, &qid);
        let pend = publishes(&out, "tower/nodes/0x0000ab12/shell/pending");
        assert!(!pend.is_empty());
        let entries: Vec<payload::PendingEntry> = serde_json::from_slice(pend[0]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].r#ref, 9);
        assert_eq!(entries[0].line, "led on");
        // 3. The node's reply chunks → shell/rsp; done clears pending.
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Shell(radio::NodeShellChunk {
                cmd_id: 1, // first allocated cmd_id
                result: 0,
                chunk: 0,
                last: true,
                text: "ok",
            }),
            &mut env,
        )
        .unwrap();
        let out = e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src: 0xAB12,
            counter: 2,
            rssi: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        }));
        let rsp_raw = publishes(&out, "tower/nodes/0x0000ab12/shell/rsp");
        assert_eq!(rsp_raw.len(), 1);
        let rsp: payload::ShellRsp = serde_json::from_slice(rsp_raw[0]).unwrap();
        assert_eq!(
            (rsp.id.as_str(), rsp.text.as_str(), rsp.done),
            ("u-1", "ok", true)
        );
        // Pending mirror emptied.
        let pend = publishes(&out, "tower/nodes/0x0000ab12/shell/pending");
        assert!(!pend.is_empty());
        let entries: Vec<payload::PendingEntry> = serde_json::from_slice(pend[0]).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn unknown_rpc_answers_error_on_rsp_topic() {
        let mut e = engine();
        let out = e.handle(Input::MqttIn {
            topic: "tower/gateway/cmd".into(),
            payload: serde_json::to_vec(&RpcRequest {
                id: "u-9".into(),
                op: "frobnicate".into(),
                params: serde_json::Value::Null,
            })
            .unwrap(),
            retain: false,
        });
        let raw = publishes(&out, "tower/gateway/rsp/u-9");
        assert_eq!(raw.len(), 1);
        let rsp: RpcResponse = serde_json::from_slice(raw[0]).unwrap();
        assert!(!rsp.ok);
    }

    #[test]
    fn first_hello_after_attach_is_a_reboot() {
        // Reality: the boot Hello of the session we attached to is consumed by the startup
        // `describe` probe, so the engine starts with NO pinned session. The first Hello it
        // ever sees therefore already means "the dongle rebooted since attach"; the old
        // test hid the bug by injecting a pinning Hello first.
        let mut e = engine();
        queue_shell(&mut e, 0xAB12, 3, "u-2", "/x");
        let out = e.handle(Input::SerialFrame(SerialMsg::Hello {
            firmware_name: "radio_dongle_gateway".into(),
            firmware_version: "v0.1.0".into(),
            session_id: 8,
        }));
        let rsp = last_shell_rsp(&out);
        assert!(rsp.error.is_some(), "the lost queue item fails loudly");
        let ops: Vec<String> = out.iter().filter_map(sent_op).map(|(_, op)| op).collect();
        assert!(ops.iter().any(|o| o.contains("Describe")));
        assert!(
            queued_ops(&e).iter().any(|o| o.contains("NodeList")),
            "next in line"
        );
        // The same session again (a console restart without a reboot) is benign…
        let out = e.handle(Input::SerialFrame(SerialMsg::Hello {
            firmware_name: "radio_dongle_gateway".into(),
            firmware_version: "v0.1.0".into(),
            session_id: 8,
        }));
        assert!(out.iter().find_map(sent_op).is_none(), "no resync");
        // …a new one is the next reboot.
        let queued = queued_ops(&e).len();
        let _ = e.handle(Input::SerialFrame(SerialMsg::Hello {
            firmware_name: "radio_dongle_gateway".into(),
            firmware_version: "v0.1.0".into(),
            session_id: 9,
        }));
        let ops = queued_ops(&e);
        assert!(
            ops.len() > queued && ops.last().unwrap().contains("NodeList"),
            "{ops:?}"
        );
    }

    #[test]
    fn mgmt_timeout_fails_the_rpc() {
        let mut e = engine();
        let out = e.handle(Input::MqttIn {
            topic: "tower/gateway/cmd".into(),
            payload: br#"{"id":"u-3","op":"node_list"}"#.to_vec(),
            retain: false,
        });
        assert!(out.iter().find_map(sent_op).is_some());
        let mut failed = false;
        for _ in 0..(MGMT_TIMEOUT_TICKS + 1) {
            let out = e.handle(Input::Tick);
            if !publishes(&out, "tower/gateway/rsp/u-3").is_empty() {
                failed = true;
            }
        }
        assert!(failed, "the pending RPC must fail after the timeout ticks");
    }

    #[test]
    fn ambient_stat_publishes_and_feeds_the_graph() {
        let mut e = engine();
        let out = e.handle(Input::SerialFrame(SerialMsg::Stat(RadioStat::Channel {
            channel: 0,
            rssi: -98,
        })));
        assert!(!publishes(&out, "tower/radio/rssi").is_empty());
        assert!(out.iter().any(|o| matches!(
            o,
            Output::Event(Event::Radio(RadioSample::Ambient { dbm: -98, .. }))
        )));
    }

    #[test]
    fn mqtt_reconnect_republishes_retained_world() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let out = e.handle(Input::MqttUp);
        assert!(!publishes(&out, "tower/gateway/status").is_empty());
        assert!(!publishes(&out, "tower/nodes/0x0000ab12").is_empty());
        assert!(!publishes(&out, "tower/nodes/0x0000ab12/shell/pending").is_empty());
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::Subscribe(f) if f == "tower/gateway/cmd"))
        );
    }

    // ---- shared helpers for the outcome-arm tests below ----

    /// Deliver one `NodeMsg::Shell` reply chunk from `src` as an uplink.
    fn shell_chunk_uplink(
        e: &mut Engine,
        src: u32,
        cmd_id: u16,
        chunk: u16,
        last: bool,
        text: &str,
    ) -> Vec<Output> {
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Shell(radio::NodeShellChunk {
                cmd_id,
                result: 0,
                chunk,
                last,
                text,
            }),
            &mut env,
        )
        .unwrap();
        e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src,
            counter: chunk as u32,
            rssi: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        }))
    }

    /// The last `ShellRsp` published to any `.../shell/rsp` topic in `out`.
    fn last_shell_rsp(out: &[Output]) -> payload::ShellRsp {
        let raw = out
            .iter()
            .rev()
            .find_map(|o| match o {
                Output::Publish { topic, payload, .. } if topic.ends_with("/shell/rsp") => {
                    Some(payload.clone())
                }
                _ => None,
            })
            .expect("a shell/rsp publish");
        serde_json::from_slice(&raw).unwrap()
    }

    /// Install a node and queue one shell command against it; returns the queue `item`.
    fn queue_shell(e: &mut Engine, id: u32, item: u16, rpc: &str, line: &str) {
        with_node(e, id, "kitchen", NODE_FLAG_SLEEPING);
        let req = format!(r#"{{"id":"{rpc}","line":"{line}"}}"#);
        let out = e.handle(Input::MqttIn {
            topic: format!("tower/nodes/{}/shell/req", topics::node_hex(id)),
            payload: req.into_bytes(),
            retain: false,
        });
        let (req_id, op) = out.iter().find_map(sent_op).expect("QueuePush goes out");
        assert!(op.contains("QueuePush"), "{op}");
        let qid = postcard::to_stdvec(&QueueId { item }).unwrap();
        let _ = reply(e, req_id, MGMT_OK, &qid);
    }

    // ---- shell response: chunk-gap truncation ----

    #[test]
    fn shell_lost_chunk_marks_truncated() {
        let mut e = engine();
        queue_shell(&mut e, 0xAB12, 9, "u-1", "/log tail");
        // Chunk 0 arrives cleanly (cmd_id 1 = the first allocated shell cmd_id).
        let out = shell_chunk_uplink(&mut e, 0xAB12, 1, 0, false, "aa");
        let rsp = last_shell_rsp(&out);
        assert_eq!((rsp.chunk, rsp.text.as_str(), rsp.truncated), (0, "aa", false));
        // Chunk 1 was CRC-dropped: chunk 2 (last) opens a gap → truncated, but the text that
        // did arrive is still delivered.
        let out = shell_chunk_uplink(&mut e, 0xAB12, 1, 2, true, "cc");
        let rsp = last_shell_rsp(&out);
        assert_eq!((rsp.chunk, rsp.text.as_str(), rsp.done), (2, "cc", true));
        assert!(
            rsp.truncated,
            "a chunk-index gap must flag the response truncated"
        );
    }

    // ---- TX outcomes on a queued shell command ----

    #[test]
    fn tx_expired_fails_pending_shell_and_clears_it() {
        let mut e = engine();
        queue_shell(&mut e, 0xAB12, 9, "u-1", "/x");
        // The gateway reports the queue item's TTL expired before it could be delivered.
        let out = e.handle(Input::SerialFrame(SerialMsg::Stat(RadioStat::Tx {
            dest: 0xAB12,
            item: 9,
            outcome: mgmt::TX_EXPIRED,
            ack_rssi: None,
        })));
        let rsp = last_shell_rsp(&out);
        assert!(rsp.done);
        assert!(
            rsp.error.as_deref().unwrap_or_default().contains("TTL expired"),
            "{:?}",
            rsp.error
        );
        // The pending mirror is cleared.
        let pend = publishes(&out, "tower/nodes/0x0000ab12/shell/pending");
        let entries: Vec<PendingEntry> = serde_json::from_slice(pend.last().unwrap()).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn tx_delivered_then_reply_timeout() {
        let mut e = engine();
        queue_shell(&mut e, 0xAB12, 9, "u-1", "/x");
        // Delivered (node ACKed): the pending stays; we now await the node's shell reply.
        let out = e.handle(Input::SerialFrame(SerialMsg::Stat(RadioStat::Tx {
            dest: 0xAB12,
            item: 9,
            outcome: mgmt::TX_DELIVERED,
            ack_rssi: Some(-50),
        })));
        assert!(
            publishes(&out, "tower/nodes/0x0000ab12/shell/rsp").is_empty(),
            "delivery alone must not answer the dialog"
        );
        // No reply within SHELL_REPLY_TIMEOUT_TICKS → a terminal timeout error.
        let mut timed_out = None;
        for _ in 0..=SHELL_REPLY_TIMEOUT_TICKS {
            let out = e.handle(Input::Tick);
            if !publishes(&out, "tower/nodes/0x0000ab12/shell/rsp").is_empty() {
                timed_out = Some(last_shell_rsp(&out));
            }
        }
        let rsp = timed_out.expect("a delivered-but-unanswered command must time out");
        assert!(rsp.done);
        assert!(
            rsp.error.as_deref().unwrap_or_default().contains("no reply"),
            "{:?}",
            rsp.error
        );
    }

    // ---- on_mgmt_done arms ----

    #[test]
    fn reveal_key_answers_with_hex_key() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let out = e.handle(Input::MqttIn {
            topic: "tower/gateway/cmd".into(),
            payload: br#"{"id":"u-rk","op":"reveal_key","params":{"addr":"0x0000ab12"}}"#.to_vec(),
            retain: false,
        });
        let (req_id, op) = out.iter().find_map(sent_op).expect("NodeRevealKey issued");
        assert!(op.contains("NodeRevealKey"), "{op}");
        let key = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
        ];
        let rec = postcard::to_stdvec(&NodeKey {
            addr: 0xAB12,
            key,
            tx_floor: 3073,
            rx_floor: 42,
        })
        .unwrap();
        let out = reply(&mut e, req_id, MGMT_OK, &rec);
        let raw = publishes(&out, "tower/gateway/rsp/u-rk");
        assert_eq!(raw.len(), 1);
        let rsp: RpcResponse = serde_json::from_slice(raw[0]).unwrap();
        assert!(rsp.ok);
        assert_eq!(rsp.data["addr"], "0x0000ab12");
        assert_eq!(rsp.data["key"], "0123456789abcdef0001020304050607");
        // The counter history travels with the key.
        assert_eq!(rsp.data["tx_floor"], 3073);
        assert_eq!(rsp.data["rx_floor"], 42);
    }

    /// `node_add` hands a moved key's counter floors to the gateway (absent = 0, a freshly
    /// minted key) and refuses floors that are not u32 numbers.
    #[test]
    fn node_add_carries_the_counter_floors() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-a1","op":"node_add","params":{"addr":"0x0000ef56","key":"00112233445566778899aabbccddeeff","tx_floor":3073,"rx_floor":42}}"#,
        ));
        let (_, op) = out.iter().find_map(sent_op).expect("NodeAdd issued");
        assert!(
            op.contains("tx_floor: 3073") && op.contains("rx_floor: 42"),
            "{op}"
        );
        // The first is in flight, so this one waits its turn in the queue (one request at a time).
        e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-a2","op":"node_add","params":{"addr":"0x0000ef57","key":"00112233445566778899aabbccddeeff"}}"#,
        ));
        let op = queued_ops(&e)
            .into_iter()
            .find(|op| op.contains("addr: 61271")) // 0x0000ef57
            .expect("NodeAdd queued");
        assert!(op.contains("tx_floor: 0") && op.contains("rx_floor: 0"), "{op}");
        for (i, bad) in [r#""12""#, "-1", "4294967296", "1.5"].into_iter().enumerate() {
            let req = format!(
                r#"{{"id":"u-b{i}","op":"node_add","params":{{"addr":"0x0000ef58","key":"00112233445566778899aabbccddeeff","tx_floor":{bad}}}}}"#
            );
            let out = e.handle(mqtt_in("tower/gateway/cmd", req.as_bytes()));
            assert!(out.iter().find_map(sent_op).is_none(), "{bad} must be refused");
            assert!(
                rpc_rsp(&out, &format!("u-b{i}"))
                    .error
                    .unwrap()
                    .contains("tx_floor")
            );
        }
    }

    fn click_uplink(src: u32, counter: u32) -> Input {
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Button {
                kind: radio::ButtonKind::Click,
                count: 1,
            },
            &mut env,
        )
        .unwrap();
        Input::SerialFrame(SerialMsg::Uplink {
            src,
            counter,
            rssi: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        })
    }

    /// Replays across a gateway reboot: the dongle's lanes are persisted lazily, so it may
    /// forward an already-seen counter again; the host publishes each counter once, in order,
    /// and a re-key (`node_add`) lets a restarted counter through.
    #[test]
    fn uplink_at_or_below_the_last_counter_is_dropped() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let button = "tower/nodes/0x0000ab12/event/button";
        assert_eq!(publishes(&e.handle(click_uplink(0xAB12, 40)), button).len(), 1);
        assert!(publishes(&e.handle(click_uplink(0xAB12, 40)), button).is_empty());
        assert!(publishes(&e.handle(click_uplink(0xAB12, 12)), button).is_empty());
        // A node reboot jumps its counter to the next reserved block: still published.
        assert_eq!(publishes(&e.handle(click_uplink(0xAB12, 1065)), button).len(), 1);
        // Re-keyed: the node may restart at 1.
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-rk2","op":"node_add","params":{"addr":"0x0000ab12","key":"00112233445566778899aabbccddeeff","name":"kitchen"}}"#,
        ));
        let (req_id, _) = out.iter().find_map(sent_op).expect("NodeAdd issued");
        reply(&mut e, req_id, MGMT_OK, &[]);
        assert_eq!(publishes(&e.handle(click_uplink(0xAB12, 1)), button).len(), 1);
    }

    #[test]
    fn node_remove_clears_retained_topics() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let out = e.handle(Input::MqttIn {
            topic: "tower/gateway/cmd".into(),
            payload: br#"{"id":"u-rm","op":"node_remove","params":{"addr":"0x0000ab12"}}"#.to_vec(),
            retain: false,
        });
        let (req_id, op) = out.iter().find_map(sent_op).expect("NodeRemove issued");
        assert!(op.contains("NodeRemove"), "{op}");
        let out = reply(&mut e, req_id, MGMT_OK, &[]);
        // An empty retained payload clears the node, its last temperature, and its pending
        // queue for any subscriber that held the retained value.
        for topic in [
            "tower/nodes/0x0000ab12",
            "tower/nodes/0x0000ab12/measure/temperature",
            "tower/nodes/0x0000ab12/shell/pending",
        ] {
            let cleared = out.iter().any(|o| {
                matches!(o, Output::Publish { topic: t, payload, retain: true }
                    if t == topic && payload.is_empty())
            });
            assert!(cleared, "missing empty retained publish for {topic}");
        }
        let rsp: RpcResponse = serde_json::from_slice(publishes(&out, "tower/gateway/rsp/u-rm")[0]).unwrap();
        assert!(rsp.ok);
    }

    #[test]
    fn ota_pairing_timeout_answers_joined_null() {
        let mut e = engine();
        let out = e.handle(Input::MqttIn {
            topic: "tower/gateway/cmd".into(),
            payload: br#"{"id":"u-ota","op":"node_add_ota","params":{"window":5}}"#.to_vec(),
            retain: false,
        });
        let (req_id, op) = out.iter().find_map(sent_op).expect("PairingOpen issued");
        assert!(op.contains("PairingOpen"), "{op}");
        // The window closed unjoined: the device answers MGMT_TIMEOUT. That is a normal
        // outcome for a pairing window, so the RPC succeeds with joined:null (not an error).
        let out = reply(&mut e, req_id, mgmt::MGMT_TIMEOUT, &[]);
        let raw = publishes(&out, "tower/gateway/rsp/u-ota");
        assert_eq!(raw.len(), 1);
        let rsp: RpcResponse = serde_json::from_slice(raw[0]).unwrap();
        assert!(rsp.ok, "MGMT_TIMEOUT on a pairing window is not an error");
        assert!(rsp.data["joined"].is_null());
    }

    /// Open an OTA window of `window` s over MQTT (RPC id `u-ota`); the PairingOpen's req_id.
    fn open_ota(e: &mut Engine, window: u16) -> u16 {
        let cmd = format!(r#"{{"id":"u-ota","op":"node_add_ota","params":{{"window":{window}}}}}"#);
        let out = e.handle(mqtt_in("tower/gateway/cmd", cmd.as_bytes()));
        let (req_id, op) = out.iter().find_map(sent_op).expect("PairingOpen issued");
        assert!(op.contains("PairingOpen"), "{op}");
        req_id
    }

    #[test]
    fn ota_join_carries_the_sas_to_the_rpc_the_pairing_topic_and_the_log() {
        let mut e = engine();
        let req_id = open_ota(&mut e, 5);
        let paired = postcard::to_stdvec(&Paired {
            addr: 0xCD34,
            sas: 4_321,
        })
        .unwrap();
        let out = reply(&mut e, req_id, MGMT_OK, &paired);
        let rsp = rpc_rsp(&out, "u-ota");
        assert!(rsp.ok);
        assert_eq!(rsp.data["joined"], "0x0000cd34");
        // Zero-padded, as both ends log it.
        assert_eq!(rsp.data["sas"], "004321");
        let pairing = publishes(&out, "tower/gateway/pairing");
        let p: payload::Pairing = serde_json::from_slice(pairing.last().unwrap()).unwrap();
        assert_eq!(p.state, "idle");
        assert_eq!(p.joined.as_deref(), Some("0x0000cd34"));
        assert_eq!(p.sas.as_deref(), Some("004321"));
        assert!(
            logs(&out)
                .iter()
                .any(|l| l.contains("SAS 004321") && l.contains("net: pairing: SAS")),
            "{:?}",
            logs(&out)
        );
    }

    #[test]
    fn ota_window_waits_out_the_pairing_slack() {
        // The gateway may answer up to PAIRING_SLACK_S after the window (a registry commit
        // absorbing an EEPROM compaction): the RPC must still be pending until then.
        let mut e = engine();
        let _ = open_ota(&mut e, 5);
        for _ in 0..(5 + PAIRING_SLACK_S - 1) {
            let out = e.handle(Input::Tick);
            assert!(
                publishes(&out, "tower/gateway/rsp/u-ota").is_empty(),
                "answered too early"
            );
        }
        let out = e.handle(Input::Tick);
        let rsp = rpc_rsp(&out, "u-ota");
        assert!(!rsp.ok, "past the slack the window fails as a device timeout");
    }

    #[test]
    fn late_paired_after_the_host_gave_up_is_reported_not_dropped() {
        let mut e = engine();
        let req_id = open_ota(&mut e, 5);
        for _ in 0..(5 + PAIRING_SLACK_S) {
            let _ = e.handle(Input::Tick);
        }
        // The gateway committed a node after all: it is registered, so it must be reported.
        let paired = postcard::to_stdvec(&Paired { addr: 0xCD34, sas: 7 }).unwrap();
        let out = reply(&mut e, req_id, MGMT_OK, &paired);
        assert!(
            logs(&out)
                .iter()
                .any(|l| l.contains("0x0000cd34") && l.contains("after") && l.contains("SAS 000007")),
            "{:?}",
            logs(&out)
        );
        let pairing = publishes(&out, "tower/gateway/pairing");
        let p: payload::Pairing = serde_json::from_slice(pairing.last().unwrap()).unwrap();
        assert_eq!(p.joined.as_deref(), Some("0x0000cd34"));
        assert_eq!(p.sas.as_deref(), Some("000007"));
        assert!(
            e.pending_mgmt.iter().any(|p| matches!(p.op, OpKind::NodeList)),
            "the registry mirror is refreshed"
        );
        assert!(e.nodes.iter().any(|n| n.entry.addr == 0xCD34), "mirrored at once");
        // Reported once: a duplicate of the same answer is stale.
        let out = reply(&mut e, req_id, MGMT_OK, &paired);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn late_window_expiry_after_the_host_gave_up_is_silent() {
        let mut e = engine();
        let req_id = open_ota(&mut e, 5);
        for _ in 0..(5 + PAIRING_SLACK_S) {
            let _ = e.handle(Input::Tick);
        }
        let out = reply(&mut e, req_id, mgmt::MGMT_TIMEOUT, &[]);
        assert!(out.is_empty(), "the expiry only confirms the timeout: {out:?}");
    }

    // ---- more uplink → topic mappings (alongside `button_uplink_maps_to_topic`) ----

    #[test]
    fn temperature_uplink_maps_to_retained_topic() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(&NodeMsg::Temperature { millic: 21500 }, &mut env).unwrap();
        let out = e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src: 0xAB12,
            counter: 3,
            rssi: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        }));
        let topic = "tower/nodes/0x0000ab12/measure/temperature";
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::Publish { topic: t, retain: true, .. } if t == topic)),
            "temperature is a retained last-value topic"
        );
        let raw = publishes(&out, topic);
        let t: payload::Temperature = serde_json::from_slice(raw[0]).unwrap();
        assert_eq!((t.celsius, t.millic), (21.5, 21500));
    }

    #[test]
    fn accel_uplink_maps_to_event_topic() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Accel {
                kind: radio::AccelKind::Motion,
                face: 0,
            },
            &mut env,
        )
        .unwrap();
        let out = e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src: 0xAB12,
            counter: 4,
            rssi: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        }));
        let topic = "tower/nodes/0x0000ab12/event/accel";
        let raw = publishes(&out, topic);
        assert_eq!(raw.len(), 1);
        let a: payload::AccelEvent = serde_json::from_slice(raw[0]).unwrap();
        assert_eq!((a.event.as_str(), a.face, a.counter), ("motion", 0, 4));
        // Accel events are transient (not retained).
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::Publish { topic: t, retain: false, .. } if t == topic))
        );
    }

    // ---- review fixes -------------------------------------------------------------

    fn mqtt_in(topic: &str, payload: &[u8]) -> Input {
        Input::MqttIn {
            topic: topic.into(),
            payload: payload.to_vec(),
            retain: false,
        }
    }

    fn logs(out: &[Output]) -> Vec<&str> {
        out.iter()
            .filter_map(|o| match o {
                Output::Event(Event::Log(l)) => Some(l.as_str()),
                _ => None,
            })
            .collect()
    }

    fn rpc_rsp(out: &[Output], id: &str) -> RpcResponse {
        let raw = publishes(out, &format!("tower/gateway/rsp/{id}"));
        assert_eq!(raw.len(), 1, "one response for {id}");
        serde_json::from_slice(raw[0]).unwrap()
    }

    #[test]
    fn describe_rpc_reports_band_and_channel() {
        // `nodes add --port` provisions the node with these; without them it defaulted
        // to band 0 / channel 0 and could never reach a us915 or channel-1/2 gateway.
        let mut e = engine();
        e.describe.band = 1;
        e.describe.channel = 2;
        let out = e.handle(mqtt_in("tower/gateway/cmd", br#"{"id":"u-d","op":"describe"}"#));
        let (req_id, _) = out.iter().find_map(sent_op).expect("Describe issued");
        // A reply that doesn't parse leaves the startup describe authoritative.
        let out = reply(&mut e, req_id, MGMT_OK, &[]);
        let rsp = rpc_rsp(&out, "u-d");
        assert_eq!(rsp.data["gateway"], "0x00000001");
        assert_eq!(
            (rsp.data["band"].as_u64(), rsp.data["channel"].as_u64()),
            (Some(1), Some(2))
        );
    }

    #[test]
    fn retained_command_is_ignored_and_cleared() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        for topic in ["tower/gateway/cmd", "tower/nodes/0x0000ab12/shell/req"] {
            let out = e.handle(Input::MqttIn {
                topic: topic.into(),
                payload: br#"{"id":"u-r","op":"node_remove","line":"x","params":{"addr":"0x0000ab12"}}"#
                    .to_vec(),
                retain: true,
            });
            assert!(
                out.iter().find_map(sent_op).is_none(),
                "a retained replay never runs"
            );
            assert!(
                out.iter().any(
                    |o| matches!(o, Output::Publish { topic: t, payload, retain: true }
                    if t == topic && payload.is_empty())
                ),
                "the stored copy is cleared on {topic}"
            );
        }
        // The clear echoing back (an empty live publish) is silently ignored.
        assert!(e.handle(mqtt_in("tower/gateway/cmd", b"")).is_empty());
    }

    #[test]
    fn malformed_and_oversized_requests_are_refused() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let long = "x".repeat(payload::MAX_REQUEST_ID + 1);
        for id in ["a/b", "+", long.as_str()] {
            let rpc = format!(r#"{{"id":"{id}","op":"node_list"}}"#);
            let out = e.handle(mqtt_in("tower/gateway/cmd", rpc.as_bytes()));
            assert!(out.iter().find_map(sent_op).is_none(), "{id:?} must not run");
            assert!(
                !out.iter().any(|o| matches!(o, Output::Publish { .. })),
                "no response topic can be built from {id:?}"
            );
            let sh = format!(r#"{{"id":"{id}","line":"x"}}"#);
            let out = e.handle(mqtt_in("tower/nodes/0x0000ab12/shell/req", sh.as_bytes()));
            assert!(out.iter().find_map(sent_op).is_none());
            assert!(!out.iter().any(|o| matches!(o, Output::Publish { .. })));
        }
        let big = format!(
            r#"{{"id":"u-big","op":"node_list","pad":"{}"}}"#,
            "y".repeat(5000)
        );
        let out = e.handle(mqtt_in("tower/gateway/cmd", big.as_bytes()));
        assert!(
            out.iter().find_map(sent_op).is_none(),
            "oversized command refused"
        );
        assert!(logs(&out).iter().any(|l| l.contains("oversized")));
    }

    #[test]
    fn duplicate_request_id_runs_once() {
        // QoS-1 is at-least-once: a redelivered request must not run twice.
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let req = br#"{"id":"u-dup","line":"led on"}"#;
        let out = e.handle(mqtt_in("tower/nodes/0x0000ab12/shell/req", req));
        assert!(out.iter().find_map(sent_op).is_some());
        let out = e.handle(mqtt_in("tower/nodes/0x0000ab12/shell/req", req));
        assert!(out.iter().find_map(sent_op).is_none(), "redelivery dropped");
        assert!(logs(&out).iter().any(|l| l.contains("duplicate")));
    }

    #[test]
    fn rpc_responses_route_by_origin_not_id() {
        let mut e = engine();
        // An MQTT client minting a TUI-looking id still gets its answer on MQTT…
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"tui-cbl-0","op":"frobnicate"}"#,
        ));
        assert!(!rpc_rsp(&out, "tui-cbl-0").ok);
        assert!(!out.iter().any(|o| matches!(o, Output::Event(Event::Rpc(_)))));
        // …and a frontend RPC with any id comes back as an event, never published.
        let out = e.handle(Input::Frontend(FrontendCmd::Rpc(RpcRequest {
            id: "anything".into(),
            op: "frobnicate".into(),
            params: serde_json::Value::Null,
        })));
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::Event(Event::Rpc(r)) if r.id == "anything"))
        );
        assert!(!out.iter().any(|o| matches!(o, Output::Publish { .. })));
    }

    #[test]
    fn unanswered_enqueue_fails_the_shell_dialog() {
        // QueuePush carries no RPC id; its timeout used to vanish silently.
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let out = e.handle(mqtt_in(
            "tower/nodes/0x0000ab12/shell/req",
            br#"{"id":"u-q","line":"x"}"#,
        ));
        assert!(out.iter().find_map(sent_op).is_some());
        let mut failed = None;
        for _ in 0..=MGMT_TIMEOUT_TICKS {
            let out = e.handle(Input::Tick);
            if !publishes(&out, "tower/nodes/0x0000ab12/shell/rsp").is_empty() {
                failed = Some(last_shell_rsp(&out));
            }
        }
        let rsp = failed.expect("a terminal shell/rsp error");
        assert_eq!(rsp.id, "u-q");
        assert!(rsp.done && rsp.error.as_deref().unwrap().contains("not queued"));
        assert!(
            e.pending_mgmt.iter().any(|p| matches!(p.op, OpKind::QueueSync)),
            "a queue resync follows, to drop the item if only the reply was lost"
        );
    }

    #[test]
    fn serial_down_fails_requests_immediately() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let _ = e.handle(Input::SerialDown {
            error: "unplugged".into(),
        });
        let out = e.handle(mqtt_in(
            "tower/nodes/0x0000ab12/shell/req",
            br#"{"id":"u-s","line":"x"}"#,
        ));
        assert!(
            !out.iter().any(|o| matches!(o, Output::SerialSend(_))),
            "frame would be dropped"
        );
        let rsp = last_shell_rsp(&out);
        assert!(rsp.error.as_deref().unwrap().contains("serial link is down"));
        let out = e.handle(mqtt_in("tower/gateway/cmd", br#"{"id":"u-l","op":"node_list"}"#));
        assert!(
            rpc_rsp(&out, "u-l")
                .error
                .unwrap()
                .contains("serial link is down")
        );
    }

    #[test]
    fn startup_queue_sync_purges_a_previous_hosts_items() {
        let mut e = engine();
        let start = e.start();
        let ops: Vec<(u16, String)> = start.iter().filter_map(sent_op).collect();
        assert_eq!(ops.len(), 1, "one request at a time: {ops:?}");
        let out = reply(&mut e, ops[0].0, MGMT_OK, &node_entry_record(0xAB12, "k", 0));
        let sync = out
            .iter()
            .find_map(sent_op)
            .expect("the queue sync follows the NodeList");
        assert!(sync.1.contains("QueueList"), "{sync:?}");
        let ops = [ops[0].clone(), sync];
        // The device still holds item 5 for 0xAB12 from a previous `tower gateway`.
        let stale = postcard::to_stdvec(&QueueEntry {
            node_addr: 0xAB12,
            item: 5,
            age: 100,
            ttl: 3600,
            data: &[1, 2, 3],
        })
        .unwrap();
        let out = reply(&mut e, ops[1].0, MGMT_OK, &stale);
        let sent: Vec<String> = out.iter().filter_map(sent_op).map(|(_, op)| op).collect();
        assert!(
            sent.iter()
                .any(|o| o.contains("QueueDrop") && o.contains("Some(5)")),
            "{sent:?}"
        );
        assert!(logs(&out).iter().any(|l| l.contains("stale")));
    }

    #[test]
    fn resync_after_serial_drop_fails_lost_commands() {
        let mut e = engine();
        queue_shell(&mut e, 0xAB12, 9, "u-1", "/x");
        let _ = e.handle(Input::SerialDown {
            error: "unplugged".into(),
        });
        let out = e.handle(Input::SerialUp);
        let (probe, op) = out.iter().find_map(sent_op).expect("SerialUp probes the dongle");
        assert!(op.contains("Describe"), "{op}");
        // Its answer frees the request slot for the queue sync.
        let out = reply(&mut e, probe, MGMT_OK, &[]);
        let sync = out
            .iter()
            .filter_map(sent_op)
            .find(|(_, op)| op.contains("QueueList"))
            .expect("SerialUp reconciles the queue");
        // The gateway no longer holds item 9: the command was lost; say so.
        let out = reply(&mut e, sync.0, MGMT_OK, &[]);
        let rsp = last_shell_rsp(&out);
        assert_eq!(rsp.id, "u-1");
        assert!(rsp.error.as_deref().unwrap().contains("no longer queued"));
        let pend = publishes(&out, "tower/nodes/0x0000ab12/shell/pending");
        let entries: Vec<PendingEntry> = serde_json::from_slice(pend.last().unwrap()).unwrap();
        assert!(entries.is_empty(), "the retained mirror is corrected");
    }

    #[test]
    fn undelivered_pending_expires_without_a_report() {
        // No TX_EXPIRED ever arrives (missed reboot / link drop): the TTL backstop fires.
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", NODE_FLAG_SLEEPING);
        let out = e.handle(mqtt_in(
            "tower/nodes/0x0000ab12/shell/req",
            br#"{"id":"u-t","line":"x","ttl":5}"#,
        ));
        let (req_id, _) = out.iter().find_map(sent_op).unwrap();
        let _ = reply(
            &mut e,
            req_id,
            MGMT_OK,
            &postcard::to_stdvec(&QueueId { item: 4 }).unwrap(),
        );
        let mut failed = None;
        for _ in 0..(5 + SHELL_TTL_GRACE_TICKS + 1) {
            let out = e.handle(Input::Tick);
            if !publishes(&out, "tower/nodes/0x0000ab12/shell/rsp").is_empty() {
                failed = Some(last_shell_rsp(&out));
            }
        }
        let rsp = failed.expect("the pending entry must not live forever");
        assert!(rsp.error.as_deref().unwrap().contains("TTL"), "{:?}", rsp.error);
    }

    #[test]
    fn queue_drop_without_ref_prunes_the_whole_mirror() {
        let mut e = engine();
        queue_shell(&mut e, 0xAB12, 9, "u-1", "/x");
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-qd","op":"queue_drop","params":{"addr":"0x0000ab12"}}"#,
        ));
        assert!(
            out.iter()
                .filter_map(sent_op)
                .any(|(_, op)| op.contains("item: None"))
        );
        let pend = publishes(&out, "tower/nodes/0x0000ab12/shell/pending");
        let entries: Vec<PendingEntry> = serde_json::from_slice(pend.last().unwrap()).unwrap();
        assert!(entries.is_empty(), "no phantom pendings");
        assert!(last_shell_rsp(&out).error.as_deref() == Some("dequeued"));
    }

    #[test]
    fn queue_drop_rejects_out_of_range_refs() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        for (i, r) in ["65545", "0", "-1", "\"9\""].iter().enumerate() {
            let id = format!("u-r{i}");
            let req =
                format!(r#"{{"id":"{id}","op":"queue_drop","params":{{"addr":"0x0000ab12","ref":{r}}}}}"#);
            let out = e.handle(mqtt_in("tower/gateway/cmd", req.as_bytes()));
            assert!(
                out.iter().find_map(sent_op).is_none(),
                "ref {r} must not truncate into a drop"
            );
            assert!(rpc_rsp(&out, &id).error.unwrap().contains("ref"));
        }
    }

    #[test]
    fn node_names_must_be_unique_printable_and_not_addresses() {
        let mut e = engine();
        let start = e.start();
        let (req_id, _) = start.iter().find_map(sent_op).unwrap();
        let recs = [
            node_entry_record(0xAB12, "kitchen", 0),
            node_entry_record(0xCD34, "garage", 0),
        ]
        .concat();
        let _ = reply(&mut e, req_id, MGMT_OK, &recs);
        for (i, name) in ["kitchen", "bad\u{1b}[2J", "rtl\u{202E}x", "0x00000001", ""]
            .iter()
            .enumerate()
        {
            let id = format!("u-n{i}");
            let req = serde_json::json!({
                "id": id, "op": "node_rename", "params": { "addr": "0x0000cd34", "name": name }
            });
            let out = e.handle(mqtt_in("tower/gateway/cmd", &serde_json::to_vec(&req).unwrap()));
            assert!(out.iter().find_map(sent_op).is_none(), "{name:?} must be refused");
            assert!(!rpc_rsp(&out, &id).ok);
        }
        // Renaming a node to its own current name is fine; so is a fresh printable name.
        for name in ["garage", "dílna 2"] {
            let req = serde_json::json!({
                "id": "u-ok", "op": "node_rename", "params": { "addr": "0x0000cd34", "name": name }
            });
            let mut e2 = engine();
            e2.nodes = e.nodes.clone();
            e2.registry_synced = true;
            let out = e2.handle(mqtt_in("tower/gateway/cmd", &serde_json::to_vec(&req).unwrap()));
            assert!(out.iter().find_map(sent_op).is_some(), "{name:?} is valid");
        }
        // node_add validates the same way.
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-add","op":"node_add","params":{"addr":"0x0000ef56","key":"00112233445566778899aabbccddeeff","name":"garage"}}"#,
        ));
        assert!(rpc_rsp(&out, "u-add").error.unwrap().contains("already used"));
    }

    fn button_uplink(src: u32) -> Input {
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Button {
                kind: radio::ButtonKind::Click,
                count: 1,
            },
            &mut env,
        )
        .unwrap();
        Input::SerialFrame(SerialMsg::Uplink {
            src,
            counter: 1,
            rssi: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        })
    }

    #[test]
    fn unregistered_uplinks_are_dropped_not_published() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let out = e.handle(button_uplink(0xCD34));
        assert!(
            !out.iter().any(|o| matches!(o, Output::Publish { .. })),
            "nothing about an unregistered sender reaches MQTT"
        );
        assert!(logs(&out).iter().any(|l| l.contains("unregistered")));
        assert!(
            out.iter()
                .filter_map(sent_op)
                .any(|(_, op)| op.contains("NodeList")),
            "the first drop re-lists in case the mirror is stale"
        );
        // Bounded noise: the 2nd..9th drops are silent, the 10th logs again.
        for _ in 2..10 {
            assert!(logs(&e.handle(button_uplink(0xCD34))).is_empty());
        }
        assert!(logs(&e.handle(button_uplink(0xCD34)))[0].contains("10 so far"));
    }

    #[test]
    fn uplinks_before_the_first_registry_sync_are_held_then_replayed() {
        let mut e = engine();
        let start = e.start();
        let (req_id, _) = start.iter().find_map(sent_op).unwrap();
        let out = e.handle(button_uplink(0xAB12));
        assert!(out.is_empty(), "held, not dropped, before the mirror exists");
        let out = reply(&mut e, req_id, MGMT_OK, &node_entry_record(0xAB12, "k", 0));
        assert_eq!(
            publishes(&out, "tower/nodes/0x0000ab12/event/button").len(),
            1,
            "replayed once the registry landed"
        );
    }

    #[test]
    fn ota_joined_node_is_mirrored_before_the_list_refresh() {
        // The joined node's first NodeInfo races the NodeList refresh: it must not be
        // dropped as unregistered (that would also skip the auto-name).
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-ota","op":"node_add_ota","params":{"window":5}}"#,
        ));
        let (req_id, _) = out.iter().find_map(sent_op).unwrap();
        let joined = postcard::to_stdvec(&Paired {
            addr: 0xCD34,
            sas: 123_456,
        })
        .unwrap();
        let _ = reply(&mut e, req_id, MGMT_OK, &joined);
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Info(radio::NodeInfo {
                firmware_name: "radio_push_button",
                firmware_version: "v0.1.0",
                session_id: 1,
                sleeping: true,
            }),
            &mut env,
        )
        .unwrap();
        let out = e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src: 0xCD34,
            counter: 1,
            rssi: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        }));
        // Queued behind the registry refresh the join triggered (one request at a time).
        assert!(
            out.iter().find_map(sent_op).is_none(),
            "the refresh holds the slot"
        );
        let op = queued_ops(&e)
            .into_iter()
            .find(|o| o.contains("NodeUpdate"))
            .expect("auto-name NodeUpdate");
        assert!(op.contains("push-button:0"), "{op}");
    }

    /// `nodes add --ota --name` renames the node right after the join, while the join's registry
    /// refresh (which still lists it unnamed) is in flight; the node's first `NodeInfo` then
    /// lands before the rename is answered. Found on the bench: the auto-name went out after
    /// the rename and overwrote it.
    #[test]
    fn a_rename_right_after_the_join_is_not_auto_named_over() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-ota","op":"node_add_ota","params":{"window":5}}"#,
        ));
        let (req_id, _) = out.iter().find_map(sent_op).unwrap();
        let joined = postcard::to_stdvec(&Paired {
            addr: 0xCD34,
            sas: 123_456,
        })
        .unwrap();
        let out = reply(&mut e, req_id, MGMT_OK, &joined);
        let (list_id, op) = out.iter().find_map(sent_op).expect("the join's registry refresh");
        assert!(op.contains("NodeList"), "{op}");
        let _ = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-ren","op":"node_rename","params":{"addr":"0x0000cd34","name":"porch"}}"#,
        ));
        // The refresh answers with the registry as it was before the rename.
        let mut registry = node_entry_record(0xAB12, "kitchen", 0);
        registry.extend(node_entry_record(0xCD34, "", NODE_FLAG_UNNAMED));
        let out = reply(&mut e, list_id, MGMT_OK, &registry);
        let (update_id, op) = out.iter().find_map(sent_op).expect("the rename goes out");
        assert!(op.contains("NodeUpdate") && op.contains("porch"), "{op}");

        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Info(radio::NodeInfo {
                firmware_name: "radio_push_button",
                firmware_version: "v0.1.0",
                session_id: 1,
                sleeping: true,
            }),
            &mut env,
        )
        .unwrap();
        let _ = e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src: 0xCD34,
            counter: 1,
            rssi: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        }));
        assert!(
            !queued_ops(&e).iter().any(|o| o.contains("push-button")),
            "no auto-name behind the rename: {:?}",
            queued_ops(&e)
        );
        // Answer everything that goes out from here (the rename, the flags update the info
        // queued, their refreshes) as the device would; nothing may auto-name the node.
        let mut out = reply(&mut e, update_id, MGMT_OK, &[]);
        assert!(rpc_rsp(&out, "u-ren").ok);
        let mut registry = node_entry_record(0xAB12, "kitchen", 0);
        registry.extend(node_entry_record(0xCD34, "porch", NODE_FLAG_SLEEPING));
        for _ in 0..8 {
            let Some((id, op)) = out.iter().find_map(sent_op) else {
                break;
            };
            assert!(!op.contains("push-button"), "auto-named over the rename: {op}");
            out = reply(
                &mut e,
                id,
                MGMT_OK,
                if op.contains("NodeList") { &registry } else { &[] },
            );
        }
        let _ = e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src: 0xCD34,
            counter: 2,
            rssi: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        }));
        assert!(queued_ops(&e).is_empty(), "{:?}", queued_ops(&e));
        assert_eq!(
            e.nodes
                .iter()
                .find(|n| n.entry.addr == 0xCD34)
                .unwrap()
                .entry
                .name,
            "porch"
        );
    }

    /// `queue_list` lists every node's queue without an `addr`, and refuses one that names no
    /// node instead of falling back to "all".
    #[test]
    fn queue_list_refuses_an_unknown_node() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"q-1","op":"queue_list","params":{"addr":"cellar"}}"#,
        ));
        let rsp = rpc_rsp(&out, "q-1");
        assert!(!rsp.ok && rsp.error.as_deref() == Some("unknown node"), "{rsp:?}");
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"q-2","op":"queue_list","params":{"addr":"kitchen"}}"#,
        ));
        let (_, op) = out.iter().find_map(sent_op).expect("a named node is listed");
        assert!(op.contains("43794"), "{op}"); // 0xAB12
    }

    /// The dongle takes management requests through a one-deep inbox, so the engine sends one
    /// at a time: the next goes out when the previous is answered or times out, a `PairingOpen`
    /// (answered only when its window ends) frees the slot after `PAIRING_PICKUP_TICKS`, and a
    /// serial drop fails what never went out.
    #[test]
    fn management_requests_go_out_one_at_a_time() {
        let mut e = engine();
        let start = e.start();
        let sent: Vec<(u16, String)> = start.iter().filter_map(sent_op).collect();
        assert_eq!(sent.len(), 1, "{sent:?}");
        let rpc = e.handle(mqtt_in("tower/gateway/cmd", br#"{"id":"u-q","op":"describe"}"#));
        assert!(rpc.iter().find_map(sent_op).is_none());
        // The rest wait in issue order and leave one by one, on answers or timeouts.
        let waiting = queued_ops(&e).len();
        assert_eq!(waiting, 2, "{:?}", queued_ops(&e));
        let out = reply(&mut e, sent[0].0, MGMT_OK, &[]);
        assert_eq!(out.iter().filter_map(sent_op).count(), 1);
        assert_eq!(queued_ops(&e).len(), waiting - 1);
        for _ in 0..MGMT_TIMEOUT_TICKS {
            let _ = e.handle(Input::Tick);
        }
        assert_eq!(queued_ops(&e).len(), waiting - 2, "a timeout frees the slot too");
        // Drain the rest, then a PairingOpen holds the slot only for its pickup time.
        while let Some(p) = e.pending_mgmt.iter().find(|p| p.frame.is_none()) {
            let req = p.req_id;
            let _ = reply(&mut e, req, MGMT_OK, &[]);
        }
        let _ = open_ota(&mut e, 30);
        let list = e.handle(mqtt_in("tower/gateway/cmd", br#"{"id":"u-l","op":"node_list"}"#));
        assert!(list.iter().find_map(sent_op).is_none(), "waits for the pickup");
        let mut sent_after = None;
        for t in 1..=PAIRING_PICKUP_TICKS {
            let out = e.handle(Input::Tick);
            if out
                .iter()
                .filter_map(sent_op)
                .any(|(_, op)| op.contains("NodeList"))
            {
                sent_after = Some(t);
                break;
            }
        }
        assert_eq!(sent_after, Some(PAIRING_PICKUP_TICKS));
        // A serial drop fails the requests that never went out, at once.
        let rpc = e.handle(mqtt_in("tower/gateway/cmd", br#"{"id":"u-d","op":"describe"}"#));
        assert!(
            rpc.iter().find_map(sent_op).is_none(),
            "queued behind the NodeList"
        );
        let out = e.handle(Input::SerialDown {
            error: "unplugged".into(),
        });
        assert!(
            !rpc_rsp(&out, "u-d").ok,
            "failed at the drop, not after a timeout"
        );
    }

    #[test]
    fn broker_errors_are_reported_even_before_the_first_connect() {
        let mut e = engine();
        let out = e.handle(Input::MqttDown {
            error: "connection refused: bad user name or password".into(),
        });
        assert!(logs(&out)[0].contains("cannot connect to the broker: connection refused"));
        let _ = e.handle(Input::MqttUp);
        let out = e.handle(Input::MqttDown { error: "eof".into() });
        assert!(logs(&out)[0].starts_with("mqtt lost"));
    }

    /// A `NodeInfo` heartbeat from `src`, running firmware `fw`, as an uplink.
    fn info_uplink(src: u32, counter: u32, fw: &str) -> Input {
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Info(radio::NodeInfo {
                firmware_name: fw,
                firmware_version: "v0.1.0",
                session_id: 1,
                sleeping: true,
            }),
            &mut env,
        )
        .unwrap();
        Input::SerialFrame(SerialMsg::Uplink {
            src,
            counter,
            rssi: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        })
    }

    /// One chunk of a successful management answer.
    fn mgmt_chunk(req_id: u16, chunk: u16, last: bool, data: &[u8]) -> Input {
        Input::SerialFrame(SerialMsg::Mgmt {
            req_id,
            result: MGMT_OK,
            chunk,
            last,
            data: data.to_vec(),
        })
    }

    /// Answer every request the engine sends, in order, as the device would: a `NodeList` with
    /// `registry`, anything else with an empty success. Bounded, so a request loop fails the test.
    fn answer_all(e: &mut Engine, registry: &[u8]) {
        for _ in 0..16 {
            let Some(p) = e.pending_mgmt.iter().find(|p| p.frame.is_none()) else {
                return;
            };
            let (req_id, list) = (p.req_id, matches!(p.op, OpKind::NodeList));
            let _ = reply(e, req_id, MGMT_OK, if list { registry } else { &[] });
        }
        panic!("the engine kept sending requests: {:?}", queued_ops(e));
    }

    /// A `NodeList` answer with a lost middle chunk can't prove a node absent, and its records
    /// are spliced from the bytes around the gap: nothing is merged (no node vanishes, no retained
    /// topic is cleared, the startup sync stays pending), the RPC fails, and the registry is
    /// listed again.
    #[test]
    fn a_node_list_with_a_lost_chunk_is_not_merged() {
        let mut e = engine();
        let a = node_entry_record(0xAB12, "kitchen", 0);
        let registry = [a.clone(), node_entry_record(0xCD34, "porch", 0)].concat();
        // Chunks 0 and 2 of a three-chunk answer; chunk 1 (the end of the first record and the
        // start of the second) was lost.
        let gapped = |e: &mut Engine, req_id: u16| {
            let _ = e.handle(mgmt_chunk(req_id, 0, false, &registry[..a.len() - 3]));
            e.handle(mgmt_chunk(req_id, 2, true, &registry[a.len() + 4..]))
        };
        let start = e.start();
        let (list_id, _) = start.iter().find_map(sent_op).unwrap();
        let out = gapped(&mut e, list_id);
        assert!(!e.registry_synced, "the startup sync has not landed");
        assert!(e.nodes.is_empty(), "{:?}", e.nodes);
        assert!(
            queued_ops(&e).iter().any(|o| o.contains("NodeList")),
            "{:?}",
            queued_ops(&e)
        );
        let (sync, op) = out.iter().find_map(sent_op).expect("the queue sync goes out");
        assert!(op.contains("QueueList"), "{op}");
        let out = reply(&mut e, sync, MGMT_OK, &[]);
        let (relist, _) = out
            .iter()
            .find_map(sent_op)
            .expect("the registry is listed again");
        let _ = reply(&mut e, relist, MGMT_OK, &registry);
        assert!(e.registry_synced);

        let out = e.handle(mqtt_in("tower/gateway/cmd", br#"{"id":"u-l","op":"node_list"}"#));
        let (list_id, _) = out.iter().find_map(sent_op).unwrap();
        let out = gapped(&mut e, list_id);
        let names: Vec<(u32, &str)> = e
            .nodes
            .iter()
            .map(|n| (n.entry.addr, n.entry.name.as_str()))
            .collect();
        assert_eq!(names, [(0xAB12, "kitchen"), (0xCD34, "porch")]);
        assert!(
            !out.iter()
                .any(|o| matches!(o, Output::Publish { payload, .. } if payload.is_empty())),
            "no retained topic is cleared: {out:?}"
        );
        assert!(!rpc_rsp(&out, "u-l").ok);
        assert!(
            out.iter()
                .filter_map(sent_op)
                .any(|(_, op)| op.contains("NodeList")),
            "the registry is listed again"
        );
    }

    /// A removal the bridge learns of only from a listing (a `node_remove` whose answer timed out
    /// although the dongle did remove the node) cleans up as the removal's answer does: each
    /// pending shell dialog ends with its one `done`, and every retained topic of the node clears.
    #[test]
    fn a_removal_seen_only_in_a_listing_cleans_up_like_node_remove() {
        let mut e = engine();
        queue_shell(&mut e, 0xAB12, 3, "u-2", "/x");
        let _ = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-rm","op":"node_remove","params":{"addr":"0x0000ab12"}}"#,
        ));
        let mut out = Vec::new();
        for _ in 0..=MGMT_TIMEOUT_TICKS {
            out.extend(e.handle(Input::Tick));
        }
        assert!(!rpc_rsp(&out, "u-rm").ok, "the removal's answer was lost");
        let out = e.handle(mqtt_in("tower/gateway/cmd", br#"{"id":"u-l","op":"node_list"}"#));
        let (list_id, _) = out.iter().find_map(sent_op).unwrap();
        let out = reply(&mut e, list_id, MGMT_OK, &[]);
        assert!(e.nodes.is_empty());
        assert_eq!(publishes(&out, "tower/nodes/0x0000ab12/shell/rsp").len(), 1);
        let rsp = last_shell_rsp(&out);
        assert_eq!(rsp.id, "u-2");
        assert!(
            rsp.done && rsp.error.as_deref() == Some("node removed"),
            "{rsp:?}"
        );
        for topic in [
            "tower/nodes/0x0000ab12",
            "tower/nodes/0x0000ab12/measure/temperature",
            "tower/nodes/0x0000ab12/shell/pending",
        ] {
            assert!(
                out.iter().any(
                    |o| matches!(o, Output::Publish { topic: t, payload, retain: true }
                    if t == topic && payload.is_empty())
                ),
                "missing empty retained publish for {topic}"
            );
        }
    }

    /// A rename refused up front (the serial link is down) never reaches the dongle, so the
    /// mirror keeps the old name.
    #[test]
    fn a_rename_refused_while_the_serial_link_is_down_is_not_mirrored() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let _ = e.handle(Input::SerialDown { error: "gone".into() });
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-ren","op":"node_rename","params":{"addr":"0x0000ab12","name":"porch"}}"#,
        ));
        assert!(!rpc_rsp(&out, "u-ren").ok);
        assert_eq!(e.nodes[0].entry.name, "kitchen");
        assert!(
            publishes(&out, "tower/nodes/0x0000ab12").is_empty(),
            "nothing renamed to publish"
        );
    }

    /// An auto-name still waiting for the request slot when the serial link drops is never sent,
    /// though the mirror took it: the reconnect lists the registry, which restores the device's
    /// truth (still unnamed), so the next heartbeat names the node for real.
    #[test]
    fn an_auto_name_lost_at_a_serial_drop_is_undone_by_the_reconnect() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "", NODE_FLAG_UNNAMED);
        // A request in flight holds the slot, so the auto-name waits.
        let out = e.handle(mqtt_in("tower/gateway/cmd", br#"{"id":"u-d","op":"describe"}"#));
        let (held, _) = out.iter().find_map(sent_op).unwrap();
        let _ = e.handle(info_uplink(0xAB12, 1, "radio_push_button"));
        assert!(
            queued_ops(&e).iter().any(|o| o.contains("push-button:0")),
            "{:?}",
            queued_ops(&e)
        );
        let _ = e.handle(Input::SerialDown { error: "gone".into() });
        let _ = e.handle(Input::SerialUp);
        let _ = reply(&mut e, held, MGMT_OK, &[]);
        answer_all(&mut e, &node_entry_record(0xAB12, "", NODE_FLAG_UNNAMED));
        let n = &e.nodes[0].entry;
        assert!(
            n.name.is_empty() && n.flags & NODE_FLAG_UNNAMED != 0,
            "the device's truth: {n:?}"
        );
        let out = e.handle(info_uplink(0xAB12, 2, "radio_push_button"));
        assert!(
            out.iter()
                .filter_map(sent_op)
                .any(|(_, op)| op.contains("push-button:0")),
            "named again"
        );
    }

    /// A rename needs the node in the mirror. The dongle's `NodeUpdate` replaces the flags, so the
    /// host sends the node's flags with `unnamed` cleared (sent without flags, the node stayed
    /// unnamed and its next heartbeat auto-named over the rename), and the duplicate-name check
    /// needs the other names. Until the startup listing lands, the node is an `unknown node`.
    #[test]
    fn a_rename_needs_the_node_mirrored_and_clears_unnamed() {
        let mut e = engine();
        let start = e.start();
        let (list_id, _) = start.iter().find_map(sent_op).unwrap();
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-ren","op":"node_rename","params":{"addr":"0x0000cd34","name":"porch"}}"#,
        ));
        assert_eq!(rpc_rsp(&out, "u-ren").error.as_deref(), Some("unknown node"));
        assert!(
            !queued_ops(&e).iter().any(|o| o.contains("NodeUpdate")),
            "{:?}",
            queued_ops(&e)
        );
        let record = node_entry_record(0xCD34, "", NODE_FLAG_UNNAMED | NODE_FLAG_SLEEPING);
        let out = reply(&mut e, list_id, MGMT_OK, &record);
        let (sync, _) = out.iter().find_map(sent_op).unwrap();
        let _ = reply(&mut e, sync, MGMT_OK, &[]);
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-ren2","op":"node_rename","params":{"addr":"0x0000cd34","name":"porch"}}"#,
        ));
        let (_, op) = out.iter().find_map(sent_op).expect("the rename goes out");
        assert!(op.contains(&format!("flags: Some({NODE_FLAG_SLEEPING})")), "{op}");
    }

    /// A cancelled window stays the dongle's until it answers the cancelled `PairingOpen`: a new
    /// `node_add_ota` is refused until then, and once that answer is in, a new window opens and
    /// shows on `gateway/pairing`.
    #[test]
    fn a_new_pairing_window_waits_for_the_cancelled_one_to_close() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-a","op":"node_add_ota","params":{"window":30}}"#,
        ));
        let (open_a, _) = out.iter().find_map(sent_op).unwrap();
        let _ = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-c","op":"pairing_cancel"}"#,
        ));
        let mut cancel = None;
        for _ in 0..PAIRING_PICKUP_TICKS {
            cancel = cancel.or(e.handle(Input::Tick).iter().find_map(sent_op));
        }
        let (cancel, op) = cancel.expect("the cancel goes out after the pickup");
        assert!(op.contains("PairingCancel"), "{op}");
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-b","op":"node_add_ota","params":{"window":30}}"#,
        ));
        assert!(!rpc_rsp(&out, "u-b").ok, "window A is still open on the dongle");
        // The dongle takes the cancel: window A resolves unjoined, then the cancel is acked.
        let out = reply(&mut e, open_a, mgmt::MGMT_TIMEOUT, &[]);
        assert!(rpc_rsp(&out, "u-a").data["joined"].is_null());
        let out = reply(&mut e, cancel, MGMT_OK, &[]);
        assert!(rpc_rsp(&out, "u-c").ok);
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-b2","op":"node_add_ota","params":{"window":30}}"#,
        ));
        assert!(
            out.iter()
                .filter_map(sent_op)
                .any(|(_, op)| op.contains("PairingOpen"))
        );
        let pairing = publishes(&out, "tower/gateway/pairing");
        let p: payload::Pairing = serde_json::from_slice(pairing.last().unwrap()).unwrap();
        assert_eq!((p.state.as_str(), p.remaining), ("open", Some(30)));
    }

    /// An auto-name fits the dongle's name limit: a long kind is clipped (on a char boundary) so
    /// `kind:n` stays within `MAX_NODE_NAME`; the dongle refused a longer one, and the node, still
    /// unnamed, was named again on every heartbeat.
    #[test]
    fn auto_names_fit_the_name_limit() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "", NODE_FLAG_UNNAMED);
        let out = e.handle(info_uplink(0xAB12, 1, "radio_climate_monitor"));
        let (_, op) = out.iter().find_map(sent_op).expect("the auto-name goes out");
        assert!(op.contains("\"climate-monito:0\""), "{op}");
        assert_eq!(e.nodes[0].entry.name, "climate-monito:0");
        for n in [0, 9, 10, 12345] {
            let name = auto_name("ěěěěěěěěěě", n);
            assert!(name.len() <= mgmt::MAX_NODE_NAME, "{name}");
            assert!(name.ends_with(&format!("ě:{n}")), "{name}");
        }
    }

    /// An unanswered `queue_drop` may not have reached the dongle, whose item would then still run
    /// with nobody listening (its dialog already ended `dequeued`): the queue is resynced, as after
    /// an unanswered enqueue, so such an item is dropped as stale.
    #[test]
    fn an_unanswered_queue_drop_resyncs_the_queue() {
        let mut e = engine();
        queue_shell(&mut e, 0xAB12, 3, "u-2", "/x");
        let out = e.handle(mqtt_in(
            "tower/gateway/cmd",
            br#"{"id":"u-d","op":"queue_drop","params":{"addr":"0x0000ab12","ref":3}}"#,
        ));
        assert_eq!(last_shell_rsp(&out).error.as_deref(), Some("dequeued"));
        let mut out = Vec::new();
        for _ in 0..=MGMT_TIMEOUT_TICKS {
            out.extend(e.handle(Input::Tick));
        }
        assert!(!rpc_rsp(&out, "u-d").ok);
        assert!(
            e.pending_mgmt.iter().any(|p| matches!(p.op, OpKind::QueueSync)),
            "a queue resync follows"
        );
    }

    /// A long streamed reply keeps its dialog alive: every arriving chunk restarts the reply
    /// timeout, which only ends a reply the node has gone quiet on.
    #[test]
    fn a_streaming_reply_is_not_cut_while_its_chunks_keep_coming() {
        let mut e = engine();
        queue_shell(&mut e, 0xAB12, 9, "u-1", "/log tail");
        let _ = e.handle(Input::SerialFrame(SerialMsg::Stat(RadioStat::Tx {
            dest: 0xAB12,
            item: 9,
            outcome: mgmt::TX_DELIVERED,
            ack_rssi: None,
        })));
        let rsp = "tower/nodes/0x0000ab12/shell/rsp";
        for chunk in 0..3 {
            for _ in 0..SHELL_REPLY_TIMEOUT_TICKS - 1 {
                assert!(
                    publishes(&e.handle(Input::Tick), rsp).is_empty(),
                    "cut mid-stream"
                );
            }
            let out = shell_chunk_uplink(&mut e, 0xAB12, 1, chunk, false, "aa");
            assert!(last_shell_rsp(&out).error.is_none());
        }
        let out = shell_chunk_uplink(&mut e, 0xAB12, 1, 3, true, "bb");
        let last = last_shell_rsp(&out);
        assert!(last.done && last.error.is_none() && !last.truncated, "{last:?}");
    }

    /// A refused enqueue answers the dialog in the shell's code space: the generic failure `1`,
    /// the dongle's reason in `error`. Its management code (`MGMT_FULL` = 4, `MGMT_BUSY` = 5)
    /// would read as another shell result (`5` is the shell's "denied").
    #[test]
    fn a_refused_enqueue_reports_the_generic_shell_failure() {
        for code in [mgmt::MGMT_FULL, mgmt::MGMT_BUSY, mgmt::MGMT_NOT_FOUND] {
            let mut e = engine();
            with_node(&mut e, 0xAB12, "kitchen", 0);
            let out = e.handle(mqtt_in(
                "tower/nodes/0x0000ab12/shell/req",
                br#"{"id":"u-q","line":"x"}"#,
            ));
            let (req_id, _) = out.iter().find_map(sent_op).unwrap();
            let out = reply(&mut e, req_id, code, &[]);
            let rsp = last_shell_rsp(&out);
            assert_eq!(rsp.result, 1, "code {code}");
            assert!(rsp.done);
            assert_eq!(rsp.error.as_deref(), payload::mgmt_error_str(code));
        }
    }
}
