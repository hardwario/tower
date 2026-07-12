//! HIL gateway group: the full wire-v3 product path on two boards — Dongle =
//! `radio_dongle_gateway`, Core = `radio_push_button`. Exercises cable provisioning
//! (typed mgmt frames on both consoles), the transparent uplink bridge (typed
//! `Frame::Uplink` with a decodable `radio::NodeMsg`), the downlink queue with the
//! ACK-pending delivery window (remote shell round-trip), and the RadioStat stream.
//!
//! The node's "finger" is its `/button simulate <ms>` shell command — the bench has no
//! actuator, and a simulated press runs the exact recognition + count/radio path a real
//! press does (a 100 ms press = a click: > debounce, < click-timeout).
//!
//! Sequential flash, sequential per-step captures. `#[ignore]`d: needs the bench +
//! two boards in RF range; run with `just hil` (`--test-threads=1`, exclusive ports).

use std::process::Command;
use std::time::Duration;

use tower_hil::{Console, Frame, bench_or_fail, firmware_dir};
use tower_protocol::mgmt::{self, DeviceInfo, DeviceRole, MgmtOp, Provision, QueueId};
use tower_protocol::radio::{self, NodeCmd, NodeMsg};

/// Build a product app (`just build app <name>`) and flash it to `port`.
fn build_and_flash_app(port: &str, app: &str) -> Result<(), String> {
    let repo = firmware_dir()?;
    let out = Command::new("just")
        .current_dir(&repo)
        .args(["build", "app", app])
        .output()
        .map_err(|e| format!("HIL: spawn just build: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "HIL: build app {app} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let bin = repo.join("target/firmware.bin");
    let out = Command::new("tower")
        .args(["-d", port, "flash"])
        .arg(&bin)
        .output()
        .map_err(|e| format!("HIL: spawn tower flash: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "HIL: flash {port} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn describe_full(console: &mut Console, req_id: u16) -> (DeviceRole, u32, u8, u8) {
    let (result, data) = console
        .mgmt_roundtrip(req_id, &MgmtOp::Describe, Duration::from_secs(4))
        .expect("Describe reply");
    assert_eq!(result, mgmt::MGMT_OK);
    let info = postcard::from_bytes::<DeviceInfo>(&data).expect("DeviceInfo record");
    (info.role, info.addr, info.band, info.channel)
}

/// The whole product story, one flow: provision over the cable, bridge a simulated
/// click, run a remote shell command through the downlink queue, see stats flow.
#[test]
#[ignore = "requires the HIL bench (Dongle=gateway + Core=push-button in RF range)"]
fn gateway_bridges_push_button_end_to_end() {
    let bench = bench_or_fail();

    build_and_flash_app(&bench.dongle.serial, "radio_dongle_gateway").expect("flash gateway");
    build_and_flash_app(&bench.core.serial, "radio_push_button").expect("flash push-button");

    let mut gw = Console::open(&bench.dongle.serial).expect("open gateway console");
    let mut node = Console::open(&bench.core.serial).expect("open node console");
    gw.reset_into_app().expect("reset gateway");
    node.reset_into_app().expect("reset node");
    gw.resync();
    node.resync();
    // Both boots settle (Hello + possible compaction stall).
    let _ = gw.wait_for(Duration::from_secs(10), |f| matches!(f, Frame::Hello { .. }));
    let _ = node.wait_for(Duration::from_secs(10), |f| matches!(f, Frame::Hello { .. }));
    std::thread::sleep(Duration::from_millis(300)); // post-Hello boot-burst guard

    // --- role probes (the same check `tower gateway` / `nodes add --port` gate on) ---
    let (gw_role, gw_addr, band, channel) = describe_full(&mut gw, 1);
    assert_eq!(gw_role, DeviceRole::Gateway);
    let (node_role, node_addr, _, _) = describe_full(&mut node, 1);
    assert_eq!(node_role, DeviceRole::Node);

    // --- cable pairing: host-minted key, gateway-first registration, then Provision ---
    let key: [u8; 16] = *b"HIL-TEST-KEY-01!";
    let (result, _) = gw
        .mgmt_roundtrip(
            2,
            &MgmtOp::NodeAdd { addr: node_addr, key, name: "bench", flags: mgmt::NODE_FLAG_SLEEPING },
            Duration::from_secs(4),
        )
        .expect("NodeAdd reply");
    assert_eq!(result, mgmt::MGMT_OK, "gateway registers the node");

    let (result, data) = node
        .mgmt_roundtrip(
            2,
            &MgmtOp::Provision(Provision { addr: None, gw_addr, key, band, channel }),
            Duration::from_secs(4),
        )
        .expect("Provision reply");
    assert_eq!(result, mgmt::MGMT_OK, "node accepts the credentials");
    assert!(!data.is_empty(), "ProvisionAck record present");

    // The node reboots into its new identity — wait for the fresh Hello.
    node.resync();
    let hello = node
        .wait_for(Duration::from_secs(12), |f| matches!(f, Frame::Hello { .. }))
        .expect("node console read");
    assert!(hello.is_some(), "node rebooted after provisioning");
    std::thread::sleep(Duration::from_millis(500));

    // --- uplink bridge: a simulated click must surface as a typed Uplink on the gateway ---
    node.send_shell(10, "/button simulate 100").expect("sim click");
    let uplink = gw
        .wait_for(Duration::from_secs(10), |f| {
            matches!(f, Frame::Uplink { src, data, .. }
                if *src == node_addr
                && matches!(radio::decode_node_msg(data), Ok(NodeMsg::Button { kind: radio::ButtonKind::Click, count: 1 })))
        })
        .expect("gateway console read");
    assert!(uplink.is_some(), "gateway forwarded the click (count=1) verbatim");

    // --- downlink queue + remote shell: enqueue, deliver on the next wake, reply ---
    let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
    let n = radio::encode_node_cmd(
        &NodeCmd::Shell { epoch: 0x4849_4C00, cmd_id: 77, line: "/system settings get therm-period" },
        &mut env,
    )
    .expect("encode NodeCmd");
    let (result, data) = gw
        .mgmt_roundtrip(3, &MgmtOp::QueuePush { node_addr, ttl: 120, data: &env[..n] }, Duration::from_secs(4))
        .expect("QueuePush reply");
    assert_eq!(result, mgmt::MGMT_OK);
    let item = postcard::from_bytes::<QueueId>(&data).expect("QueueId").item;
    assert_ne!(item, 0);

    // Wake the node: another simulated click. Its uplink's ACK carries the pending
    // flag; the gateway delivers; the node executes and streams the reply back.
    node.send_shell(11, "/button simulate 100").expect("sim click #2");
    let delivered = gw
        .wait_for(Duration::from_secs(10), |f| {
            matches!(f, Frame::Stat(tower_protocol::msg::RadioStat::Tx { item: i, outcome, .. })
                if *i == item && *outcome == mgmt::TX_DELIVERED)
        })
        .expect("gateway console read");
    assert!(delivered.is_some(), "queued downlink delivered into the wake window");

    let reply = gw
        .wait_for(Duration::from_secs(10), |f| {
            matches!(f, Frame::Uplink { src, data, .. }
                if *src == node_addr
                && matches!(radio::decode_node_msg(data), Ok(NodeMsg::Shell(c)) if c.cmd_id == 77 && c.last))
        })
        .expect("gateway console read");
    assert!(reply.is_some(), "remote shell reply chunks bridged back");

    // --- ambient stats stream (default 1 Hz) ---
    let stat = gw
        .wait_for(Duration::from_secs(5), |f| {
            matches!(f, Frame::Stat(tower_protocol::msg::RadioStat::Channel { .. }))
        })
        .expect("gateway console read");
    assert!(stat.is_some(), "channel RSSI stats flowing");

    assert_eq!(gw.seq_gaps(), 0, "no frames lost on the gateway console");
}
