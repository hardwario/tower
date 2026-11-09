//! `tower net …`: gateway/network health, over MQTT (retained-topic reads; scripts
//! get a liveness exit code without owning any serial port).

use std::time::Duration;

use anyhow::Result;
use clap::Subcommand;

use crate::gateway::payload::{self, Stats, Status};
use crate::gateway::topics;
use crate::term::sanitize_line;
use crate::{EXIT_ERROR, EXIT_OK, EXIT_PROTOCOL_MISMATCH};

use super::{MqttOpts, connect};

#[derive(Subcommand, Debug)]
pub(crate) enum NetCmd {
    /// Gateway health: link state, identity, node count, radio snapshot.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Reboot the gateway: the bridge pulses the dongle's reset line (as `tower reset` does,
    /// through the port it holds); waits until the gateway is back with a new session.
    Reboot,
}

/// How long `tower net reboot` waits for the gateway's new session (it boots in a few seconds).
const REBOOT_WAIT: Duration = Duration::from_secs(20);

pub(crate) fn run(cmd: NetCmd, opts: MqttOpts) -> Result<u8> {
    match cmd {
        NetCmd::Status { json } => status(&opts, json),
        NetCmd::Reboot => reboot(&opts),
    }
}

fn reboot(opts: &MqttOpts) -> Result<u8> {
    let mut s = connect(opts)?;
    let topic = topics::gateway_status(&s.prefix.clone());
    let session = |raw: &[u8]| {
        serde_json::from_slice::<Status>(raw)
            .ok()
            .filter(|st| st.state == "online" && st.serial_up)
            .map(|st| st.session_id)
    };
    let before = s
        .read_retained(&topic, Duration::from_secs(2))?
        .and_then(|raw| session(&raw));
    s.rpc_ok("reboot", serde_json::Value::Null)?;
    eprintln!("[tower] gateway rebooting…");
    let back = s.wait_published(&topic, REBOOT_WAIT, |raw| {
        session(raw).is_some_and(|id| Some(id) != before)
    })?;
    match back.as_deref().and_then(session) {
        Some(id) => {
            println!("gateway back, session {id}");
            Ok(EXIT_OK)
        }
        None => {
            eprintln!(
                "[tower] no new gateway session within {} s; see `tower net status`",
                REBOOT_WAIT.as_secs()
            );
            Ok(EXIT_ERROR)
        }
    }
}

fn status(opts: &MqttOpts, json: bool) -> Result<u8> {
    let mut s = connect(opts)?;
    let prefix = s.prefix.clone();
    let window = Duration::from_secs(2);
    let Some(raw) = s.read_retained(&topics::gateway_status(&prefix), window)? else {
        eprintln!(
            "[tower] no retained gateway status under \"{prefix}\"; no gateway has ever bridged to this broker"
        );
        return Ok(EXIT_ERROR);
    };
    let status: Status = serde_json::from_slice(&raw)?;
    if status.schema != payload::SCHEMA {
        eprintln!(
            "[tower] gateway publishes MQTT schema v{}, this build speaks v{}; update one side",
            status.schema,
            payload::SCHEMA
        );
        return Ok(EXIT_PROTOCOL_MISMATCH);
    }
    let stats: Option<Stats> = s
        .read_retained(&topics::gateway_stats(&prefix), window)?
        .and_then(|raw| serde_json::from_slice(&raw).ok());

    if json {
        let mut v = serde_json::to_value(&status)?;
        if let Some(st) = &stats {
            v["stats"] = serde_json::to_value(st)?;
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        if status.state == "offline" {
            // The LWT fired: the broker only knows we're gone; the other fields
            // are its defaults, not data. Say the one thing that's true.
            println!("state:    offline (the gateway's last-will fired; the bridge process is gone)");
        } else {
            // Retained-topic text anyone with broker access can plant: escape it.
            // The bridge takes the version from the startup `Describe` (`DeviceInfo` carries
            // it), so it is known even when no boot Hello was seen; a blank is still tolerated
            // (a planted or truncated payload): show the version when known instead of a blank
            // with a trailing space.
            let firmware = if status.firmware_version.is_empty() {
                sanitize_line(&status.firmware).into_owned()
            } else {
                format!(
                    "{} {}",
                    sanitize_line(&status.firmware),
                    sanitize_line(&status.firmware_version)
                )
            };
            println!("gateway:  {} ({firmware})", sanitize_line(&status.gateway));
            println!(
                "state:    {} (serial {} on {})",
                sanitize_line(&status.state),
                if status.serial_up { "up" } else { "DOWN" },
                sanitize_line(&status.serial_port)
            );
        }
        println!(
            "wire:     {} · mqtt schema v{}",
            sanitize_line(&status.wire),
            status.schema
        );
        if let Some(st) = &stats {
            println!(
                "network:  {} node(s), {} uplink(s), {} queued downlink(s)",
                st.nodes, st.uplinks, st.queued
            );
            if let (Some(dbm), Some(ch)) = (st.rssi, st.channel) {
                println!("radio:    ch {ch} ambient {dbm} dBm");
            }
        }
    }
    // Scripts branch on liveness: offline (LWT fired) exits non-zero.
    Ok(if status.state == "offline" {
        EXIT_ERROR
    } else {
        EXIT_OK
    })
}
