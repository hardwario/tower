//! Cable pairing (`tower nodes add --port <SERIAL>`): the one client flow that
//! touches BOTH transports: only this process can reach the node's USB port, so the
//! provisioning runs here, not on the gateway:
//!
//! 1. probe the node's port (`Describe` → role must be Node),
//! 2. learn the gateway's network parameters (RPC `describe` over MQTT),
//! 3. **mint the AES key host-side** (the only CSPRNG in the system),
//! 4. register the node on the gateway (RPC `node_add` with id+key),
//! 5. install the credentials on the node (`Provision` over its serial port: a
//!    typed frame, never a shell line, so the key stays out of history/logs);
//!    the node acks and reboots into its new identity.
//!
//! A failure at step 5 rolls the gateway registration back (step 4′), so no phantom
//! peer is left behind. Steps 1-3 touch nothing.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tower_protocol::FrameDecoder;
use tower_protocol::mgmt::{DeviceRole, MgmtOp, Provision};

use crate::gateway::topics;
use crate::mgmt::{DescribeOutcome, MgmtOutcome, describe, mgmt_roundtrip};
use crate::port::open_console;
use crate::render::warn_mismatch;
use crate::term::sanitize_line;
use crate::{EXIT_DEVICE_TIMEOUT, EXIT_OK, EXIT_PROTOCOL_MISMATCH, EXIT_WRONG_FIRMWARE};

use super::{Session, key_hex, mint_key};

pub(crate) fn run(s: &mut Session, node_port: &str, name: Option<&str>) -> Result<u8> {
    // 1. Probe the node.
    eprintln!("[tower] probing {node_port}…");
    let mut sp = open_console(node_port, false)?;
    let mut dec = FrameDecoder::new();
    let node_info = match describe(&mut *sp, &mut dec, 0x7100, Duration::from_secs(4)) {
        DescribeOutcome::Info(info) => info,
        DescribeOutcome::Refused(code) => {
            eprintln!("[tower] {node_port}: refused the role probe (code {code})");
            return Ok(EXIT_WRONG_FIRMWARE);
        }
        // A node built from another checkout (its frames carry another schema tag): refuse
        // before provisioning anything.
        DescribeOutcome::Mismatch(m) => {
            warn_mismatch(&m);
            eprintln!("[tower] {node_port}: {}; {}", m.what(), m.remedy());
            return Ok(EXIT_PROTOCOL_MISMATCH);
        }
        DescribeOutcome::Timeout => {
            eprintln!("[tower] {node_port}: no answer; is this a TOWER node?");
            return Ok(EXIT_DEVICE_TIMEOUT);
        }
        DescribeOutcome::Failed(e) => {
            bail!("{node_port}: serial link failed during the role probe: {e}")
        }
    };
    if node_info.role != DeviceRole::Node {
        eprintln!(
            "[tower] {node_port} runs \"{}\" (role {:?}): that's not a node; use the gateway's own port with `tower gateway`",
            sanitize_line(&node_info.firmware_name),
            node_info.role
        );
        return Ok(EXIT_WRONG_FIRMWARE);
    }
    let node_addr = node_info.addr;
    eprintln!(
        "[tower] node {} (\"{}\"){}",
        topics::node_hex(node_addr),
        sanitize_line(&node_info.firmware_name),
        if node_info.provisioned {
            ": already provisioned, re-pairing"
        } else {
            ""
        }
    );

    // 2. The gateway's network parameters (band/channel/id).
    let (gw_addr, band, channel) = gw_params(&s.rpc_ok("describe", serde_json::Value::Null)?)?;
    let gw_hex = topics::node_hex(gw_addr);

    // 3. Mint the key.
    let key = mint_key()?;

    // 4. Register on the gateway first (a node provisioned toward a gateway that
    //    doesn't know it would uplink into rejection).
    s.rpc_ok(
        "node_add",
        serde_json::json!({
            "addr": topics::node_hex(node_addr),
            "key": key_hex(&key),
            "name": name.unwrap_or(""),
            "sleeping": true,
        }),
    )
    .context("registering the node on the gateway")?;

    // 5. Install the credentials on the node. It reboots on success.
    let outcome = mgmt_roundtrip(
        &mut *sp,
        &mut dec,
        0x7101,
        &MgmtOp::Provision(Provision {
            addr: None,
            gw_addr,
            key,
            band,
            channel,
        }),
        Duration::from_secs(4),
    );
    match outcome {
        MgmtOutcome::Reply(r) if r.result == tower_protocol::mgmt::MGMT_OK => {
            println!(
                "paired {} → gateway {gw_hex}{}",
                topics::node_hex(node_addr),
                name.map(|n| format!(" as \"{}\"", sanitize_line(n)))
                    .unwrap_or_default()
            );
            eprintln!("[tower] node is rebooting into its new identity");
            Ok(EXIT_OK)
        }
        other => {
            // Roll the gateway registration back: no phantom peers.
            let _ = s.rpc(
                "node_remove",
                serde_json::json!({ "addr": topics::node_hex(node_addr) }),
            );
            match other {
                MgmtOutcome::Reply(r) => {
                    bail!(
                        "node refused provisioning ({}); gateway registration rolled back",
                        crate::gateway::payload::mgmt_error_str(r.result).unwrap_or("unknown")
                    )
                }
                MgmtOutcome::Timeout { .. } => {
                    bail!("node did not ack provisioning; gateway registration rolled back")
                }
                MgmtOutcome::Failed(e) => {
                    bail!("serial link to the node failed ({e}); gateway registration rolled back")
                }
            }
        }
    }
}

/// The gateway's network parameters from its `describe` RPC answer: address, band and
/// channel, all REQUIRED. A node provisioned onto a guessed band/channel can never reach
/// its gateway (and a guessed EU868 band would transmit out-of-band in the US), so an
/// answer without them fails the pairing instead of defaulting to 0.
fn gw_params(gw: &serde_json::Value) -> Result<(u32, u8, u8)> {
    let addr = gw
        .get("gateway")
        .and_then(|v| v.as_str())
        .and_then(topics::parse_node_hex)
        .context("gateway describe: missing/bad address")?;
    let field = |k: &str| -> Result<u8> {
        gw.get(k)
            .and_then(|v| v.as_u64())
            .and_then(|v| u8::try_from(v).ok())
            .with_context(|| {
                format!("gateway describe: no radio {k} in the answer; update the `tower gateway` host")
            })
    };
    Ok((addr, field("band")?, field("channel")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gw_params_requires_band_and_channel() {
        let ok = serde_json::json!({ "gateway": "0x0000ab12", "band": 1, "channel": 2 });
        assert_eq!(gw_params(&ok).unwrap(), (0xAB12, 1, 2));
        // The old describe answer carried only the address: refuse, never default to 0.
        let old = serde_json::json!({ "gateway": "0x0000ab12" });
        assert!(gw_params(&old).unwrap_err().to_string().contains("band"));
        let no_ch = serde_json::json!({ "gateway": "0x0000ab12", "band": 0 });
        assert!(gw_params(&no_ch).unwrap_err().to_string().contains("channel"));
        let bad = serde_json::json!({ "gateway": "0x0000ab12", "band": 300, "channel": 0 });
        assert!(gw_params(&bad).is_err(), "out-of-range band");
    }
}
