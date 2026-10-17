//! `tower nodes …`: node management against a running gateway, over MQTT.
//!
//! Everything printed here came over the broker (node names, kinds, shell output, error
//! strings, even "the gateway's" replies, since any client can publish on a response topic),
//! so every plain-terminal path goes through `term::sanitize*`. `--json` output is
//! serde-escaped and left byte-exact for machines.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use rumqttc::{Event as MqttEvent, Incoming, QoS};

use crate::gateway::engine::PAIRING_SLACK_S;
use crate::gateway::payload::{Node, PendingEntry, ShellReq, ShellRsp};
use crate::gateway::topics;
use crate::term::{sanitize, sanitize_line};
use crate::{EXIT_DEVICE_TIMEOUT, EXIT_ERROR, EXIT_OK};

use super::{MqttOpts, RpcTimeout, Session, connect};

#[derive(Subcommand, Debug)]
pub(crate) enum NodesCmd {
    /// List the gateway's registered nodes.
    List {
        /// Emit the raw JSON registry instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show one node (add --keys to reveal its AES key).
    Show {
        /// Node id (0xHHHHHHHH) or name.
        node: String,
        /// Also reveal the node's AES key (it is never shown otherwise).
        #[arg(long)]
        keys: bool,
        #[arg(long)]
        json: bool,
    },
    /// Pair a new node: over the air (--ota) or over its USB cable (--port).
    Add {
        /// Open the gateway's OTA pairing window (put the node in join mode: hold
        /// its button).
        #[arg(long, conflicts_with = "port")]
        ota: bool,
        /// Cable pairing: the serial port of the NODE to provision.
        #[arg(long, value_name = "SERIAL")]
        port: Option<String>,
        /// OTA window length in seconds.
        #[arg(long, value_name = "S", default_value_t = 60, requires = "ota")]
        window: u16,
        /// Friendly name to assign (default: auto-named from the node's firmware).
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove (unpair) a node.
    Remove { node: String },
    /// Rename a node.
    Rename { node: String, name: String },
    /// Run a remote shell command on a node (queued until it wakes).
    Shell {
        node: String,
        /// The command line, e.g. "/system settings set therm-period=30".
        line: String,
        /// Queue TTL in seconds (0 = gateway default, 1 h).
        #[arg(long, value_name = "S", default_value_t = 0)]
        ttl: u16,
        /// Idle wait for the node's reply in ms (sleeping nodes answer on their
        /// next wake; size this to the node's heartbeat if it rarely stirs).
        #[arg(long, value_name = "MS", default_value_t = 120_000)]
        timeout: u64,
        /// Enqueue and exit without waiting for the reply.
        #[arg(long)]
        no_wait: bool,
    },
    /// List a node's pending remote commands (queued on the gateway, or delivered and awaiting the
    /// node's reply).
    Pending {
        node: String,
        #[arg(long)]
        json: bool,
    },
    /// Drop a queued remote command by its ref (see `nodes pending`).
    Dequeue { node: String, r#ref: u16 },
}

/// Map an anyhow error to the exit-code contract (RpcTimeout → 124).
fn exit_of(e: &anyhow::Error) -> u8 {
    if e.downcast_ref::<RpcTimeout>().is_some() {
        EXIT_DEVICE_TIMEOUT
    } else {
        EXIT_ERROR
    }
}

pub(crate) fn run(cmd: NodesCmd, opts: MqttOpts) -> Result<u8> {
    match dispatch(cmd, &opts) {
        Ok(code) => Ok(code),
        Err(e) => {
            // Gateway error strings ride the broker: escape before they hit the terminal.
            eprintln!("[tower] error: {}", sanitize_line(&format!("{e:#}")));
            Ok(exit_of(&e))
        }
    }
}

fn dispatch(cmd: NodesCmd, opts: &MqttOpts) -> Result<u8> {
    let mut s = connect(opts)?;
    match cmd {
        NodesCmd::List { json } => {
            let nodes = node_list(&mut s)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&nodes)?);
            } else {
                print_table(&nodes);
            }
            Ok(EXIT_OK)
        }
        NodesCmd::Show { node, keys, json } => {
            let id = s.resolve_node(&node)?;
            let nodes = node_list(&mut s)?;
            let Some(n) = nodes.iter().find(|n| n.addr == topics::node_hex(id)) else {
                bail!("node vanished from the registry");
            };
            // The key with the counter floors that must travel with it (a `node_add` of this key
            // on another gateway passes them back: without them that gateway could repeat the
            // key's nonces). A snapshot: read it when the key moves, from the live gateway.
            let key = if keys {
                let data = s.rpc_ok("reveal_key", serde_json::json!({ "addr": topics::node_hex(id) }))?;
                let floor = |name: &str| data.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
                data.get("key")
                    .and_then(|v| v.as_str())
                    .map(|k| (k.to_string(), floor("tx_floor"), floor("rx_floor")))
            } else {
                None
            };
            if json {
                let mut v = serde_json::to_value(n)?;
                if let Some((k, tx_floor, rx_floor)) = &key {
                    v["key"] = serde_json::Value::String(k.clone());
                    v["tx_floor"] = (*tx_floor).into();
                    v["rx_floor"] = (*rx_floor).into();
                }
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print_table(std::slice::from_ref(n));
                if let Some((k, tx_floor, rx_floor)) = &key {
                    eprintln!("[tower] key material follows; treat this line as a secret");
                    println!("key: {}", sanitize_line(k));
                    println!("tx_floor: {tx_floor}  rx_floor: {rx_floor}");
                }
            }
            Ok(EXIT_OK)
        }
        NodesCmd::Add {
            ota: _,
            port: Some(port),
            name,
            ..
        } => super::pair_cable::run(&mut s, &port, name.as_deref()),
        NodesCmd::Add {
            ota: true,
            port: None,
            window,
            name,
        } => add_ota(&mut s, window, name.as_deref()),
        NodesCmd::Add { .. } => bail!("pick a pairing path: --ota or --port <SERIAL>"),
        NodesCmd::Remove { node } => {
            let id = s.resolve_node(&node)?;
            s.rpc_ok("node_remove", serde_json::json!({ "addr": topics::node_hex(id) }))?;
            println!("removed {}", topics::node_hex(id));
            Ok(EXIT_OK)
        }
        NodesCmd::Rename { node, name } => {
            let id = s.resolve_node(&node)?;
            s.rpc_ok(
                "node_rename",
                serde_json::json!({ "addr": topics::node_hex(id), "name": name }),
            )?;
            println!("renamed {} to \"{}\"", topics::node_hex(id), sanitize_line(&name));
            Ok(EXIT_OK)
        }
        NodesCmd::Shell {
            node,
            line,
            ttl,
            timeout,
            no_wait,
        } => shell(&mut s, &node, &line, ttl, Duration::from_millis(timeout), no_wait),
        NodesCmd::Pending { node, json } => {
            let id = s.resolve_node(&node)?;
            let raw = s
                .read_retained(
                    &topics::node_shell_pending(&s.prefix.clone(), id),
                    Duration::from_secs(2),
                )?
                .unwrap_or_else(|| b"[]".to_vec());
            let entries: Vec<PendingEntry> = serde_json::from_slice(&raw).unwrap_or_default();
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                println!("no queued commands");
            } else {
                println!("{:<6} {:<38} LINE", "REF", "ID");
                for e in &entries {
                    println!(
                        "{:<6} {:<38} {}",
                        e.r#ref,
                        sanitize_line(&e.id),
                        sanitize_line(&e.line)
                    );
                }
            }
            Ok(EXIT_OK)
        }
        NodesCmd::Dequeue { node, r#ref } => {
            let id = s.resolve_node(&node)?;
            s.rpc_ok(
                "queue_drop",
                serde_json::json!({ "addr": topics::node_hex(id), "ref": r#ref }),
            )?;
            println!("dequeued #{ref}", ref = r#ref);
            Ok(EXIT_OK)
        }
    }
}

fn node_list(s: &mut Session) -> Result<Vec<Node>> {
    let data = s.rpc_ok("node_list", serde_json::Value::Null)?;
    Ok(serde_json::from_value(data)?)
}

fn print_table(nodes: &[Node]) {
    if nodes.is_empty() {
        println!("no nodes paired (try `tower nodes add --ota` or `--port <SERIAL>`)");
        return;
    }
    println!("{}", table_header());
    for n in nodes {
        println!("{}", table_row(n));
    }
}

fn table_header() -> String {
    format!(
        "{:<12} {:<16} {:<12} {:>9} {:>6} {:>8} {:>5} {:<5}",
        "ADDR", "NAME", "TYPE", "LAST-SEEN", "RSSI", "UPLINKS", "PEND", "SLEEP"
    )
}

/// One registry row, every string field escaped (names/kinds are broker-supplied).
fn table_row(n: &Node) -> String {
    format!(
        "{:<12} {:<16} {:<12} {:>9} {:>6} {:>8} {:>5} {:<5}",
        sanitize_line(&n.addr),
        sanitize_line(if n.name.is_empty() { "-" } else { &n.name }),
        sanitize_line(if n.kind.is_empty() { "?" } else { &n.kind }),
        n.last_seen
            .map(|s| format!("{s}s"))
            .unwrap_or_else(|| "never".into()),
        n.rssi.map(|r| format!("{r}dBm")).unwrap_or_else(|| "-".into()),
        n.uplinks,
        n.queued,
        if n.sleeping { "yes" } else { "no" },
    )
}

/// What to do with the SAS `nodes add --ota` printed for the node `addr`.
fn sas_hint(addr: &str) -> String {
    format!(
        "[tower] compare this SAS with the one the node logged (`net: pairing: SAS ...`, also in its \
         `JOINED gateway` line); they must match. A mismatch means a man in the middle: remove the \
         node (`tower nodes remove {addr}`) and pair it again."
    )
}

fn add_ota(s: &mut Session, window: u16, name: Option<&str>) -> Result<u8> {
    eprintln!("[tower] pairing window open for {window}s: hold the node's button to join…");
    eprintln!(
        "[tower] to check the pairing's SAS, watch the node's console meanwhile (USB attached: \
         `tower -d <node port> logs`)"
    );
    // The RPC resolves when the window does (join / expiry), up to the bridge's pairing slack
    // after it (`PAIRING_SLACK_S`); wait that plus a margin for the broker round trip.
    let wait = Duration::from_secs(window as u64 + PAIRING_SLACK_S + 5);
    let saved = std::mem::replace(&mut s.timeout, wait);
    let rsp = s.rpc("node_add_ota", serde_json::json!({ "window": window }));
    s.timeout = saved;
    let data = match rsp {
        Ok(r) if r.ok => r.data,
        Ok(r) => bail!("{}", r.error.unwrap_or_else(|| "gateway refused".into())),
        Err(e) => return Err(e),
    };
    match data.get("joined").and_then(|v| v.as_str()) {
        Some(hex) => {
            let shown = sanitize_line(hex);
            match data.get("sas").and_then(|v| v.as_str()) {
                Some(sas) => {
                    println!("paired {shown} (SAS {})", sanitize_line(sas));
                    eprintln!("{}", sas_hint(&shown));
                }
                None => println!("paired {shown}"),
            }
            if let Some(n) = name {
                s.rpc_ok("node_rename", serde_json::json!({ "addr": hex, "name": n }))?;
                println!("named \"{n}\"");
            }
            Ok(EXIT_OK)
        }
        None => {
            eprintln!("[tower] window expired; no node joined");
            Ok(EXIT_ERROR)
        }
    }
}

fn shell(s: &mut Session, node: &str, line: &str, ttl: u16, timeout: Duration, no_wait: bool) -> Result<u8> {
    let id = s.resolve_node(node)?;
    let req_id = uuid::Uuid::new_v4().to_string();
    let rsp_topic = topics::node_shell_rsp(&s.prefix.clone(), id);
    if !no_wait {
        s.client
            .subscribe(&rsp_topic, QoS::AtLeastOnce)
            .context("subscribing to the response topic")?;
    }
    s.client
        .publish(
            topics::node_shell_req(&s.prefix.clone(), id),
            QoS::AtLeastOnce,
            false,
            serde_json::to_vec(&ShellReq {
                id: req_id.clone(),
                line: line.to_string(),
                ttl,
            })?,
        )
        .context("publishing the shell request")?;
    if no_wait {
        // Not confirmed queued yet: the gateway answers a failed enqueue on the same topic.
        println!(
            "sent for {} (its reply, or a queueing error, will publish on {rsp_topic})",
            topics::node_hex(id)
        );
        return Ok(EXIT_OK);
    }
    eprintln!("[tower] sent; waiting for the node to wake (Ctrl-C to stop waiting)…");
    let mut result = 0u8;
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // Say what is actually true: ask the gateway's retained queue mirror whether
            // the command is still pending (it used to claim "stays queued" even for a
            // command the gateway had silently lost).
            let pending = s
                .read_retained(
                    &topics::node_shell_pending(&s.prefix.clone(), id),
                    Duration::from_secs(2),
                )
                .ok()
                .flatten()
                .and_then(|raw| serde_json::from_slice::<Vec<PendingEntry>>(&raw).ok())
                .and_then(|v| v.into_iter().find(|e| e.id == req_id));
            match pending {
                Some(e) => eprintln!(
                    "[tower] no reply within the wait window; still pending on the gateway as ref {r} \
                     (queued, or delivered and awaiting the node's reply; drop it with \
                     `tower nodes dequeue {} {r}`)",
                    sanitize_line(node),
                    r = e.r#ref
                ),
                None => eprintln!(
                    "[tower] no reply within the wait window, and the gateway has no pending \
                     record of the command (lost, or answered while we weren't listening)"
                ),
            }
            return Ok(EXIT_DEVICE_TIMEOUT);
        }
        match s.connection.recv_timeout(remaining) {
            Ok(Ok(MqttEvent::Incoming(Incoming::Publish(p)))) if p.topic == rsp_topic => {
                let Ok(rsp) = serde_json::from_slice::<ShellRsp>(&p.payload) else {
                    continue;
                };
                if rsp.id != req_id {
                    continue; // someone else's dialog with the same node
                }
                if let Some(err) = &rsp.error {
                    eprintln!("[tower] {}", sanitize_line(err));
                    return Ok(EXIT_ERROR);
                }
                // Node output keeps its newlines/tabs; terminal controls are escaped.
                print!("{}", sanitize(&rsp.text));
                if rsp.truncated {
                    eprintln!("[tower] response truncated (chunk lost over the air)");
                    result = result.max(1);
                }
                if rsp.done {
                    use std::io::Write as _;
                    let _ = std::io::stdout().flush();
                    // Clamp like `tower exec`: device results own 1..=123.
                    return Ok(match rsp.result.max(result) {
                        0 => EXIT_OK,
                        r => r.clamp(1, 123),
                    });
                }
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => bail!("broker connection lost: {e}"),
            Err(_) => continue, // recv_timeout tick; loop decides via the deadline
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_row_escapes_broker_supplied_text() {
        let n = Node {
            addr: "0x0000ab12".into(),
            name: "evil\x1b]52;c;aGk=\x07\n[tower] ok".into(),
            kind: "push\u{202E}button".into(),
            sleeping: true,
            unnamed: false,
            last_seen: Some(3),
            rssi: Some(-67),
            uplinks: 12,
            queued: 1,
        };
        let row = table_row(&n);
        assert!(!row.contains('\x1b') && !row.contains('\x07') && !row.contains('\n'));
        assert!(!row.contains('\u{202E}'));
        assert!(row.contains("evil\\x1b]52"), "{row}");
        // Plain names still line up in their columns.
        let plain = Node {
            name: "kitchen".into(),
            kind: "push-button".into(),
            ..n
        };
        assert!(table_row(&plain).starts_with("0x0000ab12   kitchen          push-button "));
        assert_eq!(table_header().find("NAME"), table_row(&plain).find("kitchen"));
    }

    #[test]
    fn sas_hint_names_the_node_log_line_and_the_removal() {
        let hint = sas_hint("0x0000ab12");
        assert!(hint.contains("net: pairing: SAS"), "{hint}");
        assert!(hint.contains("tower nodes remove 0x0000ab12"), "{hint}");
    }
}
