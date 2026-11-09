//! HIL sleeping-node group: Dongle = `radio_gateway`, Core = `radio_button`, a taf **sleeping**
//! node (battery powered: its radio wakes only to send, and it hears a downlink in the window
//! right after an uplink; docs/firmware/taf.md, "Classes"). Two paths the other groups leave out
//! (they pair over the cable and wake the node with a simulated click):
//!
//! 1. **OTA pairing, without hands.** The gateway opens its window (`PairingOpen`, what `tower
//!    nodes add --ota` sends) and the node runs its join (`JoinOpen`: the power-up gesture's
//!    pairing, the commit round included). Both ends must name each other with one SAS, the
//!    registry must hold the node as a sleeping one, and the node's `Info` must arrive under the
//!    new key.
//! 2. **A `set` for a node that sleeps.** Queued on the gateway, it stays there until the node's
//!    next uplink of its own (no click, no console command), goes out in that uplink's window, is
//!    stored by the node and confirmed by its settings report.
//!
//! How long a set can wait is the node's longest silence. A sleeping node samples every
//! `report-period` and sends a value that moved by `report-delta`, or one whose last send is a
//! `heartbeat` old: with nothing moving, the heartbeat bounds it (60 s at the least). The test
//! runs the node at `heartbeat` 60 s and `report-period` 5 s and holds the set to their sum plus
//! a margin; the node's settings are put back afterwards (the bench board keeps its EEPROM).
//!
//! The node is left paired and registered as "bench", as the cable groups leave it.
//!
//! `#[ignore]`d: needs the bench + two boards in RF range; run with `just bench`.

use std::time::{Duration, Instant};

use tower_hil::{Console, Frame, Role, bench_or_fail, build_image, frame_text};
use tower_protocol::mgmt::{self, DeviceInfo, DeviceRole, Joined, MgmtOp, NodeEntry, Paired, QueueId};
use tower_protocol::msg::RadioStat;
use tower_protocol::props::{self, common};
use tower_protocol::radio::{self, NodeCmd, NodeMsg};

/// The node's settings for the run: the shortest heartbeat, a short sampling period.
const HEARTBEAT_S: u64 = 60;
const REPORT_PERIOD_S: u64 = 5;
/// Queued on the gateway to confirmed by the node's report: the node's longest silence (a value
/// goes out again once its last send is a heartbeat old, looked at every report period), plus
/// the exchange itself (the uplink, the downlink in its window, the settings report) and slack.
const SET_BUDGET: Duration = Duration::from_secs(HEARTBEAT_S + REPORT_PERIOD_S + 10);

fn describe(console: &mut Console, req_id: u16) -> (DeviceRole, u32) {
    // 10 s: the first mgmt op after a reset (see gateway.rs).
    let (result, data) = console
        .mgmt_roundtrip(req_id, &MgmtOp::Describe, Duration::from_secs(10))
        .expect("Describe reply");
    assert_eq!(result, mgmt::MGMT_OK);
    let info = postcard::from_bytes::<DeviceInfo>(&data).expect("DeviceInfo record");
    (info.role, info.addr)
}

/// The gateway's registry entry of `addr`, if it has one.
fn registered(gw: &mut Console, req_id: u16, addr: u32) -> Option<(String, u8, u8, u8)> {
    let (result, data) = gw
        .mgmt_roundtrip(req_id, &MgmtOp::NodeList, Duration::from_secs(6))
        .expect("NodeList reply");
    assert_eq!(result, mgmt::MGMT_OK);
    let mut rest = &data[..];
    while let Ok((entry, tail)) = postcard::take_from_bytes::<NodeEntry>(rest) {
        if entry.addr == addr {
            return Some((entry.name.to_owned(), entry.flags, entry.kind, entry.queued));
        }
        rest = tail;
    }
    None
}

/// `system settings get <name>` on the console: the value as the shell prints it
/// (`heartbeat = 900 [uint 60..=86400, default 900]`).
fn setting(node: &mut Console, cmd_id: u16, name: &str) -> String {
    let (result, text) = node
        .shell_roundtrip(
            cmd_id,
            &format!("system settings get {name}"),
            Duration::from_secs(4),
        )
        .expect("settings get reply");
    assert_eq!(result, 0, "settings get {name}: {text}");
    text.split(" = ")
        .nth(1)
        .and_then(|v| v.split_whitespace().next())
        .unwrap_or_else(|| panic!("no value in {text:?}"))
        .to_owned()
}

fn set_setting(node: &mut Console, cmd_id: u16, name: &str, value: &str) -> u8 {
    node.shell_roundtrip(
        cmd_id,
        &format!("system settings set {name} {value}"),
        Duration::from_secs(4),
    )
    .expect("settings set reply")
    .0
}

/// Read the gateway console until the node sent nothing for `quiet`: its boot, pairing and
/// settings bursts are over and it sleeps.
fn await_quiet(gw: &mut Console, node_addr: u32, quiet: Duration, within: Duration) {
    let give_up = Instant::now() + within;
    loop {
        let frames = gw.collect_for(quiet).expect("gateway console read");
        if !frames
            .iter()
            .any(|f| matches!(f, Frame::Uplink { src, .. } if *src == node_addr))
        {
            return;
        }
        assert!(Instant::now() < give_up, "the node never went quiet");
    }
}

#[test]
#[ignore = "requires the HIL bench (Dongle=gateway + Core=radio_button in RF range)"]
fn sleeping_node_pairs_over_the_air_and_takes_a_set_on_its_next_uplink() {
    let bench = bench_or_fail();
    let gw_bin = build_image("app", "radio_gateway", "").expect("build gateway");
    bench.flash(Role::Dongle, &gw_bin).expect("flash gateway");
    let node_bin = build_image("app", "radio_button", "").expect("build radio_button");
    bench.flash(Role::Core, &node_bin).expect("flash radio_button");

    let mut gw = Console::open(&bench.dongle.serial).expect("open gateway console");
    let mut node = Console::open(&bench.core.serial).expect("open node console");
    gw.reset_into_app().expect("reset gateway");
    node.reset_into_app().expect("reset node");
    let _ = gw.wait_for(Duration::from_secs(10), |f| matches!(f, Frame::Hello { .. }));
    let _ = node.wait_for(Duration::from_secs(10), |f| matches!(f, Frame::Hello { .. }));
    std::thread::sleep(Duration::from_millis(300)); // post-Hello boot-burst guard

    let (gw_role, gw_addr) = describe(&mut gw, 1);
    assert_eq!(gw_role, DeviceRole::Gateway);
    let (node_role, node_addr) = describe(&mut node, 1);
    assert_eq!(node_role, DeviceRole::Node);

    // --- 1. OTA pairing ---
    // A gateway refuses the join of a node it has registered (an OTA join never replaces a key),
    // and an earlier run, or the bench's own bridge, left this board registered: remove it. The
    // node may still hold that pairing; it keeps it only until the new one commits.
    let (result, _) = gw
        .mgmt_roundtrip(2, &MgmtOp::NodeRemove { addr: node_addr }, Duration::from_secs(6))
        .expect("NodeRemove reply");
    assert!(
        result == mgmt::MGMT_OK || result == mgmt::MGMT_NOT_FOUND,
        "NodeRemove: result {result}"
    );

    // The window's reply comes when a node joined (or the window ended): sent, not awaited. The
    // gateway computes the window's first ephemeral before it listens (about 1.2 s) and says so.
    gw.mgmt_send(3, &MgmtOp::PairingOpen { window: 45 })
        .expect("send PairingOpen");
    let open = gw
        .wait_for(Duration::from_secs(10), |f| {
            frame_text(f).is_some_and(|t| t.contains("pairing window open"))
        })
        .expect("gateway console read");
    assert!(open.is_some(), "the gateway opened its pairing window");

    // The node's side, as its power-up gesture runs it: the handshake, then the commit round (an
    // `Info` under the new key that the gateway must acknowledge). Answered when both are done.
    let t0 = Instant::now();
    let (result, data) = node
        .mgmt_roundtrip(3, &MgmtOp::JoinOpen { window: 30 }, Duration::from_secs(60))
        .expect("JoinOpen reply");
    assert_eq!(result, mgmt::MGMT_OK, "the node joined and committed");
    let joined = postcard::from_bytes::<Joined>(&data).expect("Joined record");
    println!("paired in {:?}, SAS {:06} on the node", t0.elapsed(), joined.sas);
    assert_eq!(joined.gw_addr, gw_addr, "the node names the gateway");

    let (result, data) = gw
        .mgmt_reply(3, Duration::from_secs(10))
        .expect("PairingOpen reply");
    assert_eq!(result, mgmt::MGMT_OK, "the gateway committed the node");
    let paired = postcard::from_bytes::<Paired>(&data).expect("Paired record");
    assert_eq!(paired.addr, node_addr, "the gateway names the node");
    assert!(paired.sas < 1_000_000, "a six-digit SAS");
    assert_eq!(paired.sas, joined.sas, "one SAS on both ends");

    // The node's `Info` under the new key: the commit uplink, or the one that follows it.
    let info = gw
        .wait_for(Duration::from_secs(15), |f| {
            matches!(f, Frame::Uplink { src, data, .. }
                if *src == node_addr
                && matches!(radio::decode_node_msg(data), Ok(NodeMsg::Info(i)) if i.sleeping))
        })
        .expect("gateway console read");
    assert!(info.is_some(), "the node's Info arrived under the new key");

    // Registered as a sleeping node, not named yet: what the host names from the `Info`.
    let (name, flags, kind, _) = registered(&mut gw, 4, node_addr).expect("the node is in the registry");
    assert_eq!(name, "", "unnamed after an OTA pairing");
    assert_eq!(flags, mgmt::NODE_FLAG_SLEEPING | mgmt::NODE_FLAG_UNNAMED);
    assert_eq!(kind, props::KIND_UNKNOWN);
    // The bridge's part on that `Info`: the name, the class, the kind.
    let (result, _) = gw
        .mgmt_roundtrip(
            5,
            &MgmtOp::NodeUpdate {
                addr: node_addr,
                name: Some("bench"),
                flags: Some(mgmt::NODE_FLAG_SLEEPING),
                kind: Some(props::button::KIND.id),
            },
            Duration::from_secs(6),
        )
        .expect("NodeUpdate reply");
    assert_eq!(result, mgmt::MGMT_OK, "the node named");

    // --- 2. A set while the node sleeps ---
    // The settings of the run, over the console (each change goes up in a settings report). `led`
    // is the one the set changes: it only turns the LED's event feedback off.
    let before = [
        ("heartbeat", setting(&mut node, 10, "heartbeat")),
        ("report-period", setting(&mut node, 11, "report-period")),
        ("led", setting(&mut node, 12, "led")),
    ];
    assert_eq!(
        set_setting(&mut node, 13, "heartbeat", &HEARTBEAT_S.to_string()),
        0
    );
    assert_eq!(
        set_setting(&mut node, 14, "report-period", &REPORT_PERIOD_S.to_string()),
        0
    );
    assert_eq!(set_setting(&mut node, 15, "led", "on"), 0);
    // Past the pairing's full report and the settings reports: the node sleeps.
    await_quiet(
        &mut gw,
        node_addr,
        Duration::from_secs(REPORT_PERIOD_S + 2),
        Duration::from_secs(60),
    );

    let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
    let n = radio::encode_node_cmd(
        &NodeCmd::Set {
            id: common::LED,
            value: 0,
        },
        &mut env,
    )
    .expect("encode NodeCmd");
    let t0 = Instant::now();
    let (result, data) = gw
        .mgmt_roundtrip(
            20,
            &MgmtOp::QueuePush {
                node_addr,
                ttl: 300,
                data: &env[..n],
            },
            Duration::from_secs(4),
        )
        .expect("QueuePush reply");
    assert_eq!(result, mgmt::MGMT_OK);
    let item = postcard::from_bytes::<QueueId>(&data).expect("QueueId").item;
    let (_, _, _, queued) = registered(&mut gw, 21, node_addr).expect("the node is in the registry");
    assert_eq!(queued, 1, "the set waits on the gateway while the node sleeps");

    // Nothing wakes the node from here: its next uplink is its own.
    let (mut woke, mut delivered, mut confirmed) = (None, None, None);
    let mut sent_asleep = false;
    while confirmed.is_none() && t0.elapsed() < SET_BUDGET {
        let left = SET_BUDGET.saturating_sub(t0.elapsed());
        let Some(frame) = gw.next(left).expect("gateway console read") else {
            break;
        };
        match &frame {
            Frame::Uplink { src, data, .. } if *src == node_addr => {
                woke.get_or_insert(t0.elapsed());
                if delivered.is_some()
                    && matches!(radio::decode_node_msg(data),
                        Ok(NodeMsg::Report(items)) if items.contains(&(common::LED, 0)))
                {
                    confirmed = Some(t0.elapsed());
                }
            }
            Frame::Stat(RadioStat::Tx { item: i, outcome, .. }) if *i == item => {
                sent_asleep |= woke.is_none();
                if *outcome == mgmt::TX_DELIVERED {
                    delivered = Some(t0.elapsed());
                } else {
                    println!("queue item {item}: outcome {outcome} at {:?}", t0.elapsed());
                }
            }
            _ => {}
        }
    }
    println!("set led: the node woke {woke:?}, delivered {delivered:?}, confirmed {confirmed:?}");
    let stored = setting(&mut node, 30, "led");

    // The board's own settings back, before anything fails (best effort: the verdict is below).
    for (i, (name, value)) in before.iter().enumerate() {
        if set_setting(&mut node, 40 + i as u16, name, value) != 0 {
            eprintln!("HIL: {name} not put back to {value}");
        }
    }

    assert!(
        !sent_asleep,
        "the gateway sent the set before the node's uplink (a sleeping node hears nothing then)"
    );
    assert!(delivered.is_some(), "the set was delivered within {SET_BUDGET:?}");
    assert!(
        confirmed.is_some(),
        "the node reported led off within {SET_BUDGET:?}"
    );
    // The shell prints a stored bool as `false` (an unset one as its default's text, `on`).
    assert!(
        matches!(stored.as_str(), "false" | "off"),
        "the node stored the setting: led = {stored}"
    );
    assert_eq!(gw.seq_gaps(), 0, "no frames lost on the gateway console");
}
