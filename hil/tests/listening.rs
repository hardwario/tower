//! HIL listening-node group: Dongle = `radio_dongle_gateway`, Core = `radio_control`, a taf
//! **listening** node (mains powered, receiver always on). The gateway sends such a node its queued
//! downlink at once instead of holding it for the node's next uplink (docs/taf.md, "Classes"):
//! here a property `Set` and a remote shell line each complete, end to end (queued on the gateway,
//! delivered, run, answered over the radio), within [`LISTEN_BUDGET`], with nothing waking the node.
//! The sleeping path is `gateway.rs`.
//!
//! The Core board needs no Power Module and no strip: the relay (P0) and strip (P1) pins drive
//! nothing on a bare board, and the test reads the relay back from the node's own report.
//!
//! `#[ignore]`d: needs the bench + two boards in RF range; run with `just bench`.

use std::time::{Duration, Instant};

use tower_hil::{Console, Frame, Role, bench_or_fail, build_image, frame_text};
use tower_protocol::mgmt::{self, DeviceInfo, DeviceRole, MgmtOp, Provision, QueueId};
use tower_protocol::msg::RadioStat;
use tower_protocol::props::control;
use tower_protocol::radio::{self, NodeCmd, NodeMsg};

/// Queued on the gateway to answered by the node. The bench measures about 0.7 s for an MQTT `set`
/// (one downlink, its ACK, the node's report); the budget leaves room for one retry.
const LISTEN_BUDGET: Duration = Duration::from_millis(2500);

fn describe(console: &mut Console, req_id: u16) -> (DeviceRole, u32, u8, u8) {
    // 10 s: the first mgmt op after a reset (see gateway.rs).
    let (result, data) = console
        .mgmt_roundtrip(req_id, &MgmtOp::Describe, Duration::from_secs(10))
        .expect("Describe reply");
    assert_eq!(result, mgmt::MGMT_OK);
    let info = postcard::from_bytes::<DeviceInfo>(&data).expect("DeviceInfo record");
    (info.role, info.addr, info.band, info.channel)
}

/// Queue `cmd` for `node` on the gateway; the queue item's id.
fn queue(gw: &mut Console, req_id: u16, node: u32, cmd: &NodeCmd<'_>) -> u16 {
    let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
    let n = radio::encode_node_cmd(cmd, &mut env).expect("encode NodeCmd");
    let (result, data) = gw
        .mgmt_roundtrip(
            req_id,
            &MgmtOp::QueuePush {
                node_addr: node,
                ttl: 60,
                data: &env[..n],
            },
            Duration::from_secs(4),
        )
        .expect("QueuePush reply");
    assert_eq!(result, mgmt::MGMT_OK);
    postcard::from_bytes::<QueueId>(&data).expect("QueueId").item
}

/// Read the gateway console from `t0` until queue item `item` was delivered and `answered` saw the
/// node's answer, or `LISTEN_BUDGET` ran out. Returns (delivered, answered) times since `t0`.
fn await_round_trip(
    gw: &mut Console,
    t0: Instant,
    item: u16,
    node: u32,
    answered: impl Fn(&NodeMsg<'_>) -> bool,
) -> (Option<Duration>, Option<Duration>) {
    let (mut delivered, mut answer) = (None, None);
    while (delivered.is_none() || answer.is_none()) && t0.elapsed() < LISTEN_BUDGET {
        let left = LISTEN_BUDGET.saturating_sub(t0.elapsed());
        let Some(frame) = gw.next(left).expect("gateway console read") else {
            break;
        };
        match &frame {
            Frame::Stat(RadioStat::Tx { item: i, outcome, .. }) if *i == item => {
                assert_eq!(
                    *outcome,
                    mgmt::TX_DELIVERED,
                    "queue item {item}: outcome {outcome}"
                );
                delivered = Some(t0.elapsed());
            }
            Frame::Uplink { src, data, .. }
                if *src == node && radio::decode_node_msg(data).is_ok_and(|m| answered(&m)) =>
            {
                answer = Some(t0.elapsed());
            }
            _ => {}
        }
    }
    (delivered, answer)
}

#[test]
#[ignore = "requires the HIL bench (Dongle=gateway + Core=radio_control in RF range)"]
fn listening_node_takes_downlinks_at_once() {
    let bench = bench_or_fail();
    let gw_bin = build_image("app", "radio_dongle_gateway", "").expect("build gateway");
    bench.flash(Role::Dongle, &gw_bin).expect("flash gateway");
    let node_bin = build_image("app", "radio_control", "").expect("build radio_control");
    bench.flash(Role::Core, &node_bin).expect("flash radio_control");

    let mut gw = Console::open(&bench.dongle.serial).expect("open gateway console");
    let mut node = Console::open(&bench.core.serial).expect("open node console");
    gw.reset_into_app().expect("reset gateway");
    node.reset_into_app().expect("reset node");
    let _ = gw.wait_for(Duration::from_secs(10), |f| matches!(f, Frame::Hello { .. }));
    let _ = node.wait_for(Duration::from_secs(10), |f| matches!(f, Frame::Hello { .. }));
    std::thread::sleep(Duration::from_millis(300)); // post-Hello boot-burst guard

    let (gw_role, gw_addr, band, channel) = describe(&mut gw, 1);
    assert_eq!(gw_role, DeviceRole::Gateway);
    let (node_role, node_addr, _, _) = describe(&mut node, 1);
    assert_eq!(node_role, DeviceRole::Node);

    // Cable pairing as `tower nodes add --port` does it, registered as a listening node (no
    // NODE_FLAG_SLEEPING: what the bridge sets from the node's `NodeInfo`).
    let mut key = [0u8; 16];
    getrandom::fill(&mut key).expect("no entropy source for the bench node key");
    let (result, _) = gw
        .mgmt_roundtrip(
            2,
            &MgmtOp::NodeAdd {
                addr: node_addr,
                key,
                name: "bench",
                flags: 0,
                tx_floor: 0,
                rx_floor: 0,
            },
            Duration::from_secs(4),
        )
        .expect("NodeAdd reply");
    assert_eq!(result, mgmt::MGMT_OK, "gateway registers the node");
    let (result, _) = node
        .mgmt_roundtrip(
            2,
            &MgmtOp::Provision(Provision {
                addr: None,
                gw_addr,
                key,
                band,
                channel,
            }),
            Duration::from_secs(4),
        )
        .expect("Provision reply");
    assert_eq!(result, mgmt::MGMT_OK, "node accepts the credentials");
    node.resync();
    let ready = node
        .wait_for(Duration::from_secs(20), |f| {
            frame_text(f).is_some_and(|t| t.contains("paired with gateway"))
        })
        .expect("node console read");
    assert!(ready.is_some(), "node app up and paired after provisioning");

    // Let the boot burst pass (the `Info` and the full report): wait for 3 s without an uplink
    // from the node, so what is measured below is the downlink alone.
    let quiet_by = Instant::now() + Duration::from_secs(30);
    loop {
        let frames = gw
            .collect_for(Duration::from_secs(3))
            .expect("gateway console read");
        if !frames
            .iter()
            .any(|f| matches!(f, Frame::Uplink { src, .. } if *src == node_addr))
        {
            break;
        }
        assert!(Instant::now() < quiet_by, "the node never went quiet after boot");
    }

    // 1. A property set (MQTT `nodes/{addr}/set/relay`): delivered at once, confirmed by the
    //    node's report of the new value.
    let t0 = Instant::now();
    let item = queue(
        &mut gw,
        10,
        node_addr,
        &NodeCmd::Set {
            id: control::RELAY,
            value: 1,
        },
    );
    let (delivered, reported) = await_round_trip(
        &mut gw,
        t0,
        item,
        node_addr,
        |m| matches!(m, NodeMsg::Report(items) if items.contains(&(control::RELAY, 1))),
    );
    println!("set relay: delivered {delivered:?}, reported {reported:?}");
    assert!(
        delivered.is_some(),
        "the set was delivered within {LISTEN_BUDGET:?}"
    );
    assert!(
        reported.is_some(),
        "the node reported the relay on within {LISTEN_BUDGET:?}"
    );

    // 2. A remote shell line (`tower nodes shell`): run and answered without a wake.
    let t0 = Instant::now();
    let item = queue(
        &mut gw,
        11,
        node_addr,
        &NodeCmd::Shell {
            epoch: 0x4849_4C01,
            cmd_id: 5,
            line: "node status",
        },
    );
    let (delivered, replied) = await_round_trip(
        &mut gw,
        t0,
        item,
        node_addr,
        |m| matches!(m, NodeMsg::Shell(c) if c.cmd_id == 5 && c.last),
    );
    println!("remote shell: delivered {delivered:?}, replied {replied:?}");
    assert!(
        delivered.is_some(),
        "the shell line was delivered within {LISTEN_BUDGET:?}"
    );
    assert!(replied.is_some(), "the node answered within {LISTEN_BUDGET:?}");

    // Leave the relay off.
    let t0 = Instant::now();
    let item = queue(
        &mut gw,
        12,
        node_addr,
        &NodeCmd::Set {
            id: control::RELAY,
            value: 0,
        },
    );
    let (_, off) = await_round_trip(
        &mut gw,
        t0,
        item,
        node_addr,
        |m| matches!(m, NodeMsg::Report(items) if items.contains(&(control::RELAY, 0))),
    );
    assert!(off.is_some(), "the relay went off again");

    assert_eq!(gw.seq_gaps(), 0, "no frames lost on the gateway console");
}
