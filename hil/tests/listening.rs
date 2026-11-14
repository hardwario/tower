//! HIL listening-node group: Dongle = `radio_gateway`, Core = `radio_control`, a taf
//! **listening** node (mains powered, receiver always on). The gateway sends such a node its queued
//! downlink at once instead of holding it for the node's next uplink (docs/firmware/taf.md, "Classes"):
//! here a property `Set` and a remote shell line each complete, end to end (queued on the gateway,
//! delivered, run, answered over the radio), within [`LISTEN_BUDGET`], with nothing waking the node.
//! The sleeping path is `gateway.rs` (a click wakes the node) and `sleeping.rs` (its own uplink).
//!
//! The band is the gateway's: its `band` setting, read with `Describe` and handed to the node with
//! its credentials, as in the other groups. On `us915` both ends hop (FCC §15.247): the node first
//! finds the gateway's hop schedule ([`LOCK_BUDGET`]) and then follows it, the gateway sends each
//! command in the current hop slot, and the group ends with a series of sets whose airtime must
//! have spread over the hop channels (`gateway status`).
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
/// US 915: a provisioned node's boot to its first uplink. It needs two passes of the gateway over
/// the rendezvous channel, a hop cycle (24 s) apart, the first up to a cycle away; the budget has
/// room for one pass it does not hear.
const LOCK_BUDGET: Duration = Duration::from_secs(100);
/// EU 868: the node's boot to its first uplink.
const BOOT_BUDGET: Duration = Duration::from_secs(20);
/// US 915: the sets of the closing series, and how long one may take when the air cost it an
/// attempt and the pause after it (at most one of the series may).
const SERIES: u16 = 24;
const SERIES_PATIENCE: Duration = Duration::from_secs(10);
/// US 915: how far the busiest hop channel's transmit airtime may stand above the mean after the
/// run. The beacons are even to one beacon (18 ms), a command's frame is about 16 ms and the run
/// sends under thirty: spread over the slots, a channel gets a few. On one fixed channel they
/// would add up to over 400 ms.
const EVEN_MARGIN_MS: u32 = 150;

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
/// node's answer, or `budget` ran out. Returns (delivered, answered) times since `t0`. An attempt
/// that was not delivered is printed, not failed: the budget is the verdict (it has room for one
/// repeat, which on US 915 is an attempt of its own in the next hop slot).
fn await_round_trip(
    gw: &mut Console,
    t0: Instant,
    item: u16,
    node: u32,
    budget: Duration,
    answered: impl Fn(&NodeMsg<'_>) -> bool,
) -> (Option<Duration>, Option<Duration>) {
    let (mut delivered, mut answer) = (None, None);
    while (delivered.is_none() || answer.is_none()) && t0.elapsed() < budget {
        let left = budget.saturating_sub(t0.elapsed());
        let Some(frame) = gw.next(left).expect("gateway console read") else {
            break;
        };
        match &frame {
            Frame::Stat(RadioStat::Tx { item: i, outcome, .. }) if *i == item => {
                if *outcome == mgmt::TX_DELIVERED {
                    delivered = Some(t0.elapsed());
                } else {
                    println!("queue item {item}: outcome {outcome} at {:?}", t0.elapsed());
                }
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

/// The number that follows `word` in `text` (`max 72 ms` gives 72).
fn number_after(text: &str, word: &str) -> Option<u32> {
    let mut words = text.split_whitespace();
    words.find(|w| *w == word)?;
    words.next()?.parse().ok()
}

/// US 915: a series of sets at uneven intervals, each in whatever hop slot is current, then the
/// gateway's own account of its transmit airtime per hop channel (`gateway status`). Every set
/// must be confirmed (all but one within the listening budget), every channel must have been
/// used, and the busiest must stand close to the mean: the commands went out on the hop schedule,
/// not on a channel of their own. Leaves the relay off.
fn even_channel_use(gw: &mut Console, node_addr: u32) {
    let (mut late, mut slowest) = (0, Duration::ZERO);
    for i in 0..SERIES {
        let value = i32::from(i % 2 == 0);
        let t0 = Instant::now();
        let item = queue(
            gw,
            100 + i,
            node_addr,
            &NodeCmd::Set {
                id: control::POWER_RELAY,
                value,
            },
        );
        let (_, reported) = await_round_trip(
            gw,
            t0,
            item,
            node_addr,
            SERIES_PATIENCE,
            |m| matches!(m, NodeMsg::Report(items) if items.contains(&(control::POWER_RELAY, value))),
        );
        let took = reported.unwrap_or_else(|| panic!("set {i} of the series was not confirmed"));
        late += u32::from(took > LISTEN_BUDGET);
        slowest = slowest.max(took);
        // Uneven pauses (0.1 to 0.7 s): the sets fall on slots all over the hop cycle.
        std::thread::sleep(Duration::from_millis(137 * u64::from(1 + i % 5)));
    }
    println!("series of {SERIES} sets: slowest {slowest:?}, {late} over {LISTEN_BUDGET:?}");
    assert!(
        late <= 1,
        "{late} sets of the series took longer than {LISTEN_BUDGET:?}"
    );

    // The gateway refreshes the figures every 10 s.
    let _ = gw
        .collect_for(Duration::from_secs(11))
        .expect("gateway console read");
    let (result, text) = gw
        .shell_roundtrip(200, "gateway status", Duration::from_secs(4))
        .expect("gateway status reply");
    assert_eq!(result, 0, "gateway status: {text}");
    let line = text
        .lines()
        .find(|l| l.starts_with("hopping:"))
        .unwrap_or_else(|| panic!("no hopping line in {text:?}"));
    println!("{line}");
    let figure = |word| number_after(line, word).unwrap_or_else(|| panic!("no `{word}` in {line:?}"));
    let (used, of, max, mean) = (figure("hopping:"), figure("of"), figure("max"), figure("mean"));
    assert_eq!(used, of, "every hop channel was transmitted on");
    assert!(
        max <= mean + EVEN_MARGIN_MS,
        "airtime per channel: max {max} ms is more than {EVEN_MARGIN_MS} ms above the mean {mean} ms"
    );
}

#[test]
#[ignore = "requires the HIL bench (Dongle=gateway + Core=radio_control in RF range)"]
fn listening_node_takes_downlinks_at_once() {
    let bench = bench_or_fail();
    let gw_bin = build_image("app", "radio_gateway", "").expect("build gateway");
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

    // The node's first uplink (its `Info`). On US 915 it comes once the node has found the
    // gateway's hop schedule, which it then follows; before that nothing reaches it.
    let hopping = band == mgmt::BAND_US915;
    let first_by = if hopping { LOCK_BUDGET } else { BOOT_BUDGET };
    let t0 = Instant::now();
    let first = gw
        .wait_for(
            first_by,
            |f| matches!(f, Frame::Uplink { src, .. } if *src == node_addr),
        )
        .expect("gateway console read");
    println!(
        "band {}: the node's first uplink {:?} after its boot",
        if hopping { "us915" } else { "eu868" },
        t0.elapsed()
    );
    assert!(first.is_some(), "the node's first uplink within {first_by:?}");

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

    // 1. A property set (MQTT `nodes/{addr}/set/power-relay`): delivered at once, confirmed by the
    //    node's report of the new value.
    let t0 = Instant::now();
    let item = queue(
        &mut gw,
        10,
        node_addr,
        &NodeCmd::Set {
            id: control::POWER_RELAY,
            value: 1,
        },
    );
    let (delivered, reported) = await_round_trip(
        &mut gw,
        t0,
        item,
        node_addr,
        LISTEN_BUDGET,
        |m| matches!(m, NodeMsg::Report(items) if items.contains(&(control::POWER_RELAY, 1))),
    );
    println!("set power-relay: delivered {delivered:?}, reported {reported:?}");
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
        LISTEN_BUDGET,
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
            id: control::POWER_RELAY,
            value: 0,
        },
    );
    let (_, off) = await_round_trip(
        &mut gw,
        t0,
        item,
        node_addr,
        LISTEN_BUDGET,
        |m| matches!(m, NodeMsg::Report(items) if items.contains(&(control::POWER_RELAY, 0))),
    );
    assert!(off.is_some(), "the relay went off again");

    if hopping {
        even_channel_use(&mut gw, node_addr);
    }

    assert_eq!(gw.seq_gaps(), 0, "no frames lost on the gateway console");
}
