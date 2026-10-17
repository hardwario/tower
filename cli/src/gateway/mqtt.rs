//! The gateway's MQTT side: the rumqttc **synchronous** client (its `Connection`
//! iterator drives the whole event loop on a plain thread: no async code in this
//! crate; tokio exists only inside these dependencies) and, behind the default-on
//! `embedded-broker` feature, an in-process rumqttd broker so bare `tower gateway`
//! needs zero infrastructure. In embedded mode the gateway's own client loops back
//! over localhost TCP: one MQTT code path regardless of mode.
//!
//! **Access policy** (embedded broker): a loopback bind (the default `127.0.0.1:1883`) is
//! anonymous *by design*: any local user or process can drive the gateway, exactly like
//! the serial port it owns. A non-loopback bind exposes node keys and the remote shell to
//! the network, so it refuses to start without credentials ([`broker_auth`]). There is no
//! TLS: for remote access, tunnel (SSH/VPN) to a loopback broker or use an external broker.

use std::collections::VecDeque;
// The bind/auth policy only exists where the embedded broker does (and in its tests).
#[cfg(any(feature = "embedded-broker", test))]
use std::collections::HashMap;
#[cfg(any(feature = "embedded-broker", test))]
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::Result;
use rumqttc::{Client, Connection, Event as MqttEvent, Incoming, LastWill, MqttOptions, QoS};

use super::engine::Input;
use super::payload;
use super::topics;

/// Largest MQTT packet (remaining length: topic + payload) any TOWER link *sends*, and the
/// embedded broker's `max_payload_size`; the two must agree, because rumqttd answers an
/// oversized publish by dropping the publisher's whole connection. The gateway's own
/// traffic is tiny (the biggest message, a full 16-node `node_list` reply, is ~3 KB).
pub(crate) const MAX_PAYLOAD: usize = 64 * 1024;

/// Largest packet a TOWER client *accepts*. Deliberately ≥ [`MAX_PAYLOAD`] (everything the
/// embedded broker forwards is receivable) with headroom for external brokers that don't
/// cap payloads: rumqttc answers an oversized inbound packet by dropping the connection, so
/// a 20 KiB publish on a subscribed command topic used to kill the gateway's link (last-will
/// "offline") and, retained, kill it again on every reconnect, forever.
pub(crate) const MAX_INCOMING: usize = 1024 * 1024;

/// rumqttc request-channel depth (publishes/subscribes waiting for the event loop).
const REQUEST_CAP: usize = 100;

// Clients must accept everything the embedded broker forwards.
const _: () = assert!(MAX_INCOMING >= MAX_PAYLOAD);

/// Env var for the MQTT password (argv is visible in `ps`; the environment is not).
pub(crate) const PASSWORD_ENV: &str = "TOWER_MQTT_PASSWORD";

/// One MQTT login (user + password). Deliberately no `Debug`: the password must never land
/// in a log line or a panic message.
#[derive(Clone)]
pub(crate) struct Credentials {
    pub user: String,
    pub password: String,
}

/// Resolve the `--mqtt-user` / `--mqtt-password` pair (each also settable by env var) into a
/// login. A user without a password is an error (it used to be silently ignored: an
/// auth-enabled broker then refused us with no hint why); so is an explicit password without
/// a user. A password that only came from the *environment* is ambient configuration and is
/// ignored without a user: exporting `TOWER_MQTT_PASSWORD` must not break a bare
/// `tower gateway` / `tower nodes list` against an anonymous loopback broker.
pub(crate) fn credentials(user: Option<&str>, password: Option<&str>) -> Result<Option<Credentials>> {
    let from_env = password.is_some() && std::env::var(PASSWORD_ENV).ok().as_deref() == password;
    pair_credentials(user, password, from_env).map_err(anyhow::Error::msg)
}

/// The pure decision behind [`credentials`].
fn pair_credentials(
    user: Option<&str>,
    password: Option<&str>,
    password_from_env: bool,
) -> std::result::Result<Option<Credentials>, String> {
    match (user, password) {
        (Some(""), _) => Err("the MQTT user (--mqtt-user / TOWER_MQTT_USER) is empty".into()),
        (Some(u), Some(p)) => Ok(Some(Credentials {
            user: u.to_string(),
            password: p.to_string(),
        })),
        (Some(_), None) => Err(format!(
            "an MQTT user (--mqtt-user / TOWER_MQTT_USER) needs a password: export {PASSWORD_ENV} \
             (preferred; argv is visible in `ps`) or pass --mqtt-password"
        )),
        (None, Some(_)) if password_from_env => Ok(None),
        (None, Some(_)) => Err("--mqtt-password needs an MQTT user (--mqtt-user / TOWER_MQTT_USER)".into()),
        (None, None) => Ok(None),
    }
}

/// Where a local client reaches a broker bound to `listen`. An unspecified bind address
/// (`0.0.0.0` / `::`) means "every interface", not a connectable destination (connecting
/// to it is platform-defined at best), so use the matching loopback instead.
#[cfg(any(feature = "embedded-broker", test))]
pub(crate) fn connect_addr(listen: SocketAddr) -> SocketAddr {
    match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listen.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), listen.port())
        }
        _ => listen,
    }
}

/// The embedded broker's access policy for a bind address (see the module docs): rumqttd's
/// static `user → password` table, `None` = anonymous. Loopback (127.0.0.0/8, `::1`,
/// incl. the IPv4-mapped form) without credentials stays anonymous; configured credentials
/// are enforced on any bind; a non-loopback bind (incl. `0.0.0.0` / `::`) without
/// credentials is refused with an actionable error.
#[cfg(any(feature = "embedded-broker", test))]
pub(crate) fn broker_auth(
    listen: SocketAddr,
    creds: Option<&Credentials>,
) -> std::result::Result<Option<HashMap<String, String>>, String> {
    match creds {
        Some(c) => Ok(Some([(c.user.clone(), c.password.clone())].into_iter().collect())),
        None if listen.ip().to_canonical().is_loopback() => Ok(None),
        None => Err(format!(
            "refusing to host the embedded MQTT broker on {listen} without authentication: a \
             non-loopback bind exposes the gateway (node keys, remote shell) to the network. \
             Set --mqtt-user and export {PASSWORD_ENV} (clients then need the same login), or \
             bind loopback (--broker 127.0.0.1:{port}). There is no TLS; for remote access use \
             an SSH tunnel / VPN or an external broker (--mqtt).",
            port = listen.port()
        )),
    }
}

/// Broker connection parameters (resolved from the `tower gateway` flags).
pub(crate) struct MqttParams {
    pub host: String,
    pub port: u16,
    pub credentials: Option<Credentials>,
    pub client_id: String,
}

/// Client options shared by the gateway and the `nodes`/`net` clients: keep-alive, the
/// packet-size caps (see [`MAX_PAYLOAD`] / [`MAX_INCOMING`]) and the login, if any.
pub(crate) fn options(
    client_id: String,
    host: String,
    port: u16,
    creds: Option<&Credentials>,
) -> MqttOptions {
    let mut opts = MqttOptions::new(client_id, host, port);
    opts.set_keep_alive(Duration::from_secs(15));
    opts.set_max_packet_size(MAX_INCOMING, MAX_PAYLOAD);
    if let Some(c) = creds {
        opts.set_credentials(c.user.clone(), c.password.clone());
    }
    opts
}

/// Build the client. The LWT publishes `gateway/status = offline` (retained) so
/// subscribers see a dead gateway without any timeout logic of their own.
pub(crate) fn client(params: &MqttParams, prefix: &str) -> (Client, Connection) {
    let mut opts = options(
        params.client_id.clone(),
        params.host.clone(),
        params.port,
        params.credentials.as_ref(),
    );
    opts.set_last_will(LastWill::new(
        topics::gateway_status(prefix),
        serde_json::to_vec(&payload::Status::offline()).unwrap_or_default(),
        QoS::AtLeastOnce,
        true,
    ));
    Client::new(opts, REQUEST_CAP)
}

/// Drive the rumqttc connection loop, translating broker events into engine inputs.
/// rumqttc reconnects internally; we report link edges and, while the link is down
/// (including *before the first connect*), each distinct connect error once, so a wrong
/// password or an unreachable broker is visible instead of failing silently forever. Runs
/// until the engine hangs up.
pub(crate) fn run(mut connection: Connection, input: Sender<Input>) {
    let mut was_up = false;
    let mut last_err: Option<String> = None;
    for event in connection.iter() {
        let ok = match event {
            Ok(MqttEvent::Incoming(Incoming::ConnAck(_))) => {
                was_up = true;
                last_err = None;
                input.send(Input::MqttUp).is_ok()
            }
            Ok(MqttEvent::Incoming(Incoming::Publish(p))) => input
                .send(Input::MqttIn {
                    topic: p.topic.clone(),
                    payload: p.payload.to_vec(),
                    // A stored retained message replayed on (re)subscribe: the engine
                    // refuses to run those as commands (live deliveries carry retain=0).
                    retain: p.retain,
                })
                .is_ok(),
            Ok(_) => true,
            Err(e) => {
                let error = e.to_string();
                let ok = if was_up || last_err.as_deref() != Some(error.as_str()) {
                    was_up = false;
                    last_err = Some(error.clone());
                    input.send(Input::MqttDown { error }).is_ok()
                } else {
                    true
                };
                // Don't spin on a refused broker: rumqttc retries on the next iteration.
                std::thread::sleep(Duration::from_secs(1));
                ok
            }
        };
        if !ok {
            return;
        }
    }
}

// ---- back-pressure-safe publishing ----------------------------------------------------

/// The broker-bound half of the rumqttc client, as the [`Publisher`] needs it: a trait so
/// the back-pressure policy is unit-testable with a fake whose queue can be "full".
pub(crate) trait Sink {
    /// Queue one QoS-1 publish without blocking; `false` = the request channel is full.
    fn try_publish(&self, topic: &str, retain: bool, payload: &[u8]) -> bool;
    /// Queue one QoS-1 subscribe without blocking; `false` = the request channel is full.
    fn try_subscribe(&self, filter: &str) -> bool;
}

impl Sink for Client {
    fn try_publish(&self, topic: &str, retain: bool, payload: &[u8]) -> bool {
        Client::try_publish(self, topic, QoS::AtLeastOnce, retain, payload.to_vec()).is_ok()
    }
    fn try_subscribe(&self, filter: &str) -> bool {
        Client::try_subscribe(self, filter, QoS::AtLeastOnce).is_ok()
    }
}

/// Non-diagnostic messages (events, RPC/shell replies) held while the client queue is
/// full; beyond this the oldest is shed.
const BACKLOG_CAP: usize = 256;

/// The engine thread's only path to the broker. rumqttc's request channel is bounded
/// ([`REQUEST_CAP`]) and nothing drains it while the broker is unreachable, so the old
/// blocking `client.publish` parked the **engine thread** (serial handling and the TUI
/// froze) within a minute of a broker outage (radio RSSI alone is 2 msg/s). Everything
/// here is `try_*` and never blocks; when the queue is full:
///
/// - **retained state** is coalesced per topic (latest value wins; that is all a retained
///   topic means) and re-sent as room appears;
/// - **diagnostics** ([`topics::is_diagnostic`]) are dropped, and not even attempted while
///   the link is known down: stale RSSI is worthless;
/// - **everything else** (events, replies) waits in a bounded FIFO, oldest shed first.
///
/// Subscribes that don't fit are retried too. [`Publisher::flush`] runs before every
/// engine batch (≥ 1 Hz via the ticker), so held items drain promptly on reconnect.
pub(crate) struct Publisher<S: Sink> {
    sink: S,
    prefix: String,
    link_up: bool,
    subs: Vec<String>,
    retained: Vec<(String, Vec<u8>)>,
    backlog: VecDeque<(String, Vec<u8>)>,
    /// Messages shed since the last recovery note.
    dropped: u64,
    notes: Vec<String>,
}

impl<S: Sink> Publisher<S> {
    pub(crate) fn new(sink: S, prefix: String) -> Self {
        Self {
            sink,
            prefix,
            link_up: false,
            subs: Vec::new(),
            retained: Vec::new(),
            backlog: VecDeque::new(),
            dropped: 0,
            notes: Vec::new(),
        }
    }

    /// Track the broker link (from the `MqttUp`/`MqttDown` inputs).
    pub(crate) fn set_link(&mut self, up: bool) {
        self.link_up = up;
    }

    pub(crate) fn subscribe(&mut self, filter: String) {
        self.flush();
        // Once anything is held, later subscribes queue behind it (order is preserved).
        if (!self.subs.is_empty() || !self.sink.try_subscribe(&filter)) && !self.subs.contains(&filter) {
            self.subs.push(filter);
        }
    }

    pub(crate) fn publish(&mut self, topic: String, payload: Vec<u8>, retain: bool) {
        self.flush();
        if retain {
            if self.retained.is_empty() && self.sink.try_publish(&topic, true, &payload) {
                return self.sent();
            }
            self.retained.retain(|(t, _)| *t != topic);
            self.retained.push((topic, payload));
        } else if topics::is_diagnostic(&self.prefix, &topic) {
            if !self.link_up {
                return; // stale before it could ever be delivered: not even a "drop"
            }
            if self.sink.try_publish(&topic, false, &payload) {
                self.sent();
            } else {
                self.shed();
            }
        } else {
            if self.backlog.is_empty() && self.sink.try_publish(&topic, false, &payload) {
                return self.sent();
            }
            if self.backlog.len() >= BACKLOG_CAP {
                self.backlog.pop_front();
                self.shed();
            }
            self.backlog.push_back((topic, payload));
        }
    }

    /// Re-offer everything held, in order, until the client queue refuses again.
    pub(crate) fn flush(&mut self) {
        let sink = &self.sink;
        self.subs.retain(|f| !sink.try_subscribe(f));
        while let Some((t, p)) = self.retained.first() {
            if !self.sink.try_publish(t, true, p) {
                return;
            }
            self.retained.remove(0);
            self.sent();
        }
        while let Some((t, p)) = self.backlog.front() {
            if !self.sink.try_publish(t, false, p) {
                return;
            }
            self.backlog.pop_front();
            self.sent();
        }
    }

    /// Operator notes (queue-full / recovered edges) for the frontend log.
    pub(crate) fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    fn shed(&mut self) {
        if self.dropped == 0 {
            self.notes.push(
                "MQTT publish queue full (broker slow or unreachable); shedding radio \
                 diagnostics and the oldest held events"
                    .into(),
            );
        }
        self.dropped += 1;
    }

    fn sent(&mut self) {
        if self.dropped > 0 && self.retained.is_empty() && self.backlog.is_empty() {
            self.notes.push(format!(
                "MQTT publish queue recovered ({} message(s) were shed)",
                self.dropped
            ));
            self.dropped = 0;
        }
    }
}

// ---- the embedded broker ----------------------------------------------------------------

/// rumqttd configuration for the embedded broker. Sized for a long-lived process: every
/// subscription *filter* gets its own in-memory commit log that rumqttd never frees on
/// unsubscribe, each holding up to `max_segment_count × max_segment_size` (it was
/// 10 × 100 MiB, up to ~1 GiB per filter). 2 × 1 MiB per filter bounds that, and with the
/// clients sharing one response filter (`topics::gateway_rsp_filter`) the filter count
/// stays bounded too. `max_payload_size` = [`MAX_PAYLOAD`], matching every client's send cap.
#[cfg(feature = "embedded-broker")]
pub(crate) fn broker_config(listen: SocketAddr, auth: Option<HashMap<String, String>>) -> rumqttd::Config {
    use rumqttd::{Config, ConnectionSettings, RouterConfig, ServerSettings};

    let router = RouterConfig {
        max_connections: 128,
        max_outgoing_packet_count: 200,
        max_segment_size: 1024 * 1024,
        max_segment_count: 2,
        ..Default::default()
    };
    let server = ServerSettings {
        name: "tower".to_string(),
        listen,
        tls: None,
        next_connection_delay_ms: 1,
        connections: ConnectionSettings {
            // TCP accept → MQTT CONNECT deadline (idle sockets can't pin connection slots).
            connection_timeout_ms: 5000,
            max_payload_size: MAX_PAYLOAD,
            max_inflight_count: 100,
            auth,
            external_auth: None,
            dynamic_filters: false,
        },
    };
    Config {
        id: 0,
        router,
        v4: Some([("tower".to_string(), server)].into_iter().collect()),
        ..Default::default()
    }
}

/// Fail early, with a usable message, if `addr` can't be bound (another broker owns it).
#[cfg(feature = "embedded-broker")]
fn probe_bind(addr: SocketAddr) -> Result<()> {
    std::net::TcpListener::bind(addr).map(drop).map_err(|e| {
        anyhow::anyhow!(
            "cannot host the embedded MQTT broker on {addr}: {e}; is another broker already \
             running there? Use it with --mqtt HOST[:PORT], or pick another --broker ADDR:PORT"
        )
    })
}

/// Start the embedded rumqttd broker on `listen` under the access policy ([`broker_auth`])
/// and return once it is **confirmed listening**. rumqttd reports a failed bind only through
/// `tracing` and then returns `Ok` from `start()`, so the old code announced "embedded MQTT
/// broker on …" and connected to whatever already owned the port. Now: probe-bind first
/// (also the loopback address a wildcard bind is reached through; BSD lets a wildcard bind
/// shadow-coexist with another process's specific one), then wait until the port accepts
/// connections while the broker thread is still alive.
#[cfg(feature = "embedded-broker")]
pub(crate) fn embedded_broker(listen: SocketAddr, creds: Option<&Credentials>) -> Result<()> {
    use std::sync::mpsc;
    use std::time::Instant;

    use anyhow::{Context, bail};

    let auth = broker_auth(listen, creds).map_err(anyhow::Error::msg)?;
    let target = connect_addr(listen);
    probe_bind(listen)?;
    if target != listen {
        probe_bind(target)?;
    }

    let mut broker = rumqttd::Broker::new(broker_config(listen, auth));
    let (died_tx, died) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("mqtt-broker".into())
        .spawn(move || {
            // `start` blocks for the broker's lifetime; returning at all means it's gone.
            let why = match broker.start() {
                Ok(()) => "its listener exited (address in use?)".to_string(),
                Err(e) => e.to_string(),
            };
            if died_tx.send(why.clone()).is_err() {
                eprintln!("[tower] embedded MQTT broker stopped: {why}");
            }
        })
        .context("spawning the embedded broker thread")?;

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(why) = died.try_recv() {
            bail!("embedded MQTT broker on {listen} failed to start: {why}");
        }
        if std::net::TcpStream::connect_timeout(&target, Duration::from_millis(200)).is_ok() {
            // rumqttd binds before it accepts, so a lost bind race has already ended its
            // thread by now; give that exit one beat to surface before trusting the port.
            return match died.recv_timeout(Duration::from_millis(100)) {
                Ok(why) => bail!("embedded MQTT broker on {listen} failed to start: {why}"),
                Err(_) => Ok(()),
            };
        }
        if Instant::now() >= deadline {
            bail!("embedded MQTT broker did not start listening on {listen} within 3 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn creds() -> Credentials {
        Credentials {
            user: "ops".into(),
            password: "s3cret".into(),
        }
    }

    #[test]
    fn credential_pairing_rules() {
        assert!(pair_credentials(None, None, false).unwrap().is_none());
        let c = pair_credentials(Some("u"), Some("p"), true).unwrap().unwrap();
        assert_eq!((c.user.as_str(), c.password.as_str()), ("u", "p"));
        // A user without a password used to be dropped silently → now an error.
        let e = pair_credentials(Some("u"), None, false).err().unwrap();
        assert!(e.contains(PASSWORD_ENV), "{e}");
        // An explicit password without a user is an error…
        assert!(pair_credentials(None, Some("p"), false).is_err());
        // …but an env-only password is ambient config and is ignored without a user.
        assert!(pair_credentials(None, Some("p"), true).unwrap().is_none());
        assert!(pair_credentials(Some(""), Some("p"), false).is_err());
    }

    #[test]
    fn connect_addr_maps_unspecified_to_loopback() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(connect_addr(a("0.0.0.0:1883")), a("127.0.0.1:1883"));
        assert_eq!(connect_addr(a("[::]:1884")), a("[::1]:1884"));
        assert_eq!(connect_addr(a("192.168.1.5:1883")), a("192.168.1.5:1883"));
        assert_eq!(connect_addr(a("127.0.0.1:1883")), a("127.0.0.1:1883"));
    }

    #[test]
    fn broker_auth_policy_loopback_anonymous_else_credentials() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        // Loopback without credentials: anonymous by design.
        for l in [
            "127.0.0.1:1883",
            "127.8.9.10:1883",
            "[::1]:1883",
            "[::ffff:127.0.0.1]:1883",
        ] {
            assert_eq!(broker_auth(a(l), None), Ok(None), "{l}");
        }
        // Any non-loopback bind (incl. the wildcards) without credentials: refused.
        for l in ["0.0.0.0:1883", "[::]:1883", "192.168.1.5:1883", "10.0.0.1:2000"] {
            let e = broker_auth(a(l), None).unwrap_err();
            assert!(e.contains("--mqtt-user") && e.contains(PASSWORD_ENV), "{e}");
            assert!(!e.contains("s3cret"));
        }
        // With credentials: enforced on any bind (loopback included).
        for l in ["0.0.0.0:1883", "127.0.0.1:1883"] {
            let t = broker_auth(a(l), Some(&creds())).unwrap().unwrap();
            assert_eq!(t.get("ops").map(String::as_str), Some("s3cret"));
            assert_eq!(t.len(), 1);
        }
    }

    #[cfg(feature = "embedded-broker")]
    #[test]
    fn broker_config_is_bounded_and_carries_auth() {
        let listen: SocketAddr = "0.0.0.0:1883".parse().unwrap();
        let auth = broker_auth(listen, Some(&creds())).unwrap();
        let cfg = broker_config(listen, auth);
        assert_eq!(cfg.router.max_segment_size, 1024 * 1024);
        assert_eq!(cfg.router.max_segment_count, 2);
        let server = &cfg.v4.as_ref().unwrap()["tower"];
        assert_eq!(server.listen, listen);
        assert!(server.tls.is_none());
        assert_eq!(server.connections.max_payload_size, MAX_PAYLOAD);
        let table = server.connections.auth.as_ref().expect("auth enabled");
        assert_eq!(table.get("ops").map(String::as_str), Some("s3cret"));
        // Loopback without credentials: no auth table.
        let lo: SocketAddr = "127.0.0.1:1883".parse().unwrap();
        let cfg = broker_config(lo, broker_auth(lo, None).unwrap());
        assert!(cfg.v4.unwrap()["tower"].connections.auth.is_none());
    }

    /// A fake rumqttc channel with a settable capacity.
    struct FakeSink {
        room: Cell<usize>,
        sent: RefCell<Vec<(String, bool, Vec<u8>)>>,
        subs: RefCell<Vec<String>>,
    }
    impl FakeSink {
        fn new(room: usize) -> Self {
            Self {
                room: Cell::new(room),
                sent: RefCell::new(Vec::new()),
                subs: RefCell::new(Vec::new()),
            }
        }
        fn take(&self) -> bool {
            if self.room.get() == 0 {
                return false;
            }
            self.room.set(self.room.get() - 1);
            true
        }
    }
    impl Sink for &FakeSink {
        fn try_publish(&self, topic: &str, retain: bool, payload: &[u8]) -> bool {
            self.take() && {
                self.sent
                    .borrow_mut()
                    .push((topic.into(), retain, payload.to_vec()));
                true
            }
        }
        fn try_subscribe(&self, filter: &str) -> bool {
            self.take() && {
                self.subs.borrow_mut().push(filter.into());
                true
            }
        }
    }

    #[test]
    fn publisher_never_blocks_and_prioritizes_state() {
        let sink = FakeSink::new(0); // broker down, client queue already full
        let mut p = Publisher::new(&sink, "tower/".into());
        p.set_link(false);
        // A minute of RSSI while down: dropped outright, never queued.
        for _ in 0..120 {
            p.publish("tower/radio/rssi".into(), b"{}".to_vec(), false);
        }
        // Retained state coalesces per topic: only the latest status survives.
        p.publish("tower/gateway/status".into(), b"old".to_vec(), true);
        p.publish("tower/gateway/status".into(), b"new".to_vec(), true);
        // Events wait in the bounded FIFO (overflow sheds the oldest).
        for i in 0..(BACKLOG_CAP + 3) {
            p.publish(
                "tower/nodes/0x00000001/event/button".into(),
                i.to_string().into_bytes(),
                false,
            );
        }
        p.subscribe("tower/gateway/cmd".into());
        assert!(sink.sent.borrow().is_empty());
        assert_eq!(p.retained.len(), 1);
        assert_eq!(p.backlog.len(), BACKLOG_CAP);
        assert_eq!(p.backlog.front().unwrap().1, b"3", "oldest three shed");
        assert!(p.take_notes()[0].contains("queue full"));

        // Link back, queue drains: subscribe first, then state, then events in order.
        p.set_link(true);
        sink.room.set(usize::MAX);
        p.flush();
        assert_eq!(sink.subs.borrow().as_slice(), ["tower/gateway/cmd"]);
        let sent = sink.sent.borrow();
        assert_eq!(sent[0], ("tower/gateway/status".into(), true, b"new".to_vec()));
        assert_eq!(sent.len(), 1 + BACKLOG_CAP);
        assert_eq!(sent[1].2, b"3");
        drop(sent);
        let notes = p.take_notes();
        assert!(
            notes.iter().any(|n| n.contains("recovered (3 message(s)")),
            "{notes:?}"
        );
    }

    #[test]
    fn publisher_keeps_order_behind_held_items() {
        let sink = FakeSink::new(0);
        let mut p = Publisher::new(&sink, "tower/".into());
        p.set_link(true);
        p.publish("tower/gateway/rsp/a".into(), b"1".to_vec(), false);
        sink.room.set(1);
        // Room for one: the held reply goes first, the new one waits behind it.
        p.publish("tower/gateway/rsp/b".into(), b"2".to_vec(), false);
        assert_eq!(sink.sent.borrow()[0].0, "tower/gateway/rsp/a");
        assert_eq!(p.backlog.len(), 1);
    }

    /// End-to-end through the real embedded rumqttd: bind, connect, retained
    /// publish/subscribe round-trip. The one integration point unit tests can't fake.
    #[cfg(feature = "embedded-broker")]
    #[test]
    fn embedded_broker_roundtrip() {
        // A fixed high port would collide across CI runs; probe for a free one.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        embedded_broker(addr, None).expect("broker confirmed listening");

        let opts = options("test-client".into(), "127.0.0.1".into(), port, None);
        let (client, mut connection) = Client::new(opts, 10);

        let mut connected = false;
        let mut got: Option<Vec<u8>> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match connection.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(MqttEvent::Incoming(Incoming::ConnAck(_)))) => {
                    connected = true;
                    client.subscribe("tower/test", rumqttc::QoS::AtLeastOnce).unwrap();
                    client
                        .publish("tower/test", rumqttc::QoS::AtLeastOnce, true, b"hi".as_slice())
                        .unwrap();
                }
                Ok(Ok(MqttEvent::Incoming(Incoming::Publish(p)))) if p.topic == "tower/test" => {
                    got = Some(p.payload.to_vec());
                    break;
                }
                _ => {}
            }
        }
        assert!(connected, "client never connected to the embedded broker");
        assert_eq!(got.as_deref(), Some(b"hi".as_slice()));
    }

    /// The bind-conflict path: a port someone else owns must fail startup loudly instead of
    /// announcing a broker and silently connecting to the squatter.
    #[cfg(feature = "embedded-broker")]
    #[test]
    fn embedded_broker_refuses_an_occupied_port() {
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = squatter.local_addr().unwrap();
        let e = embedded_broker(addr, None).unwrap_err().to_string();
        assert!(e.contains("cannot host the embedded MQTT broker"), "{e}");
        assert!(e.contains("--mqtt"), "actionable: {e}");
    }

    /// Auth end-to-end: an auth-enabled embedded broker admits the configured login and
    /// drops an anonymous client.
    #[cfg(feature = "embedded-broker")]
    #[test]
    fn embedded_broker_enforces_credentials() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        embedded_broker(addr, Some(&creds())).expect("broker up");

        let connack = |c: Option<&Credentials>, id: &str| -> bool {
            let (_client, mut conn) = Client::new(options(id.into(), "127.0.0.1".into(), port, c), 10);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                match conn.recv_timeout(Duration::from_millis(500)) {
                    Ok(Ok(MqttEvent::Incoming(Incoming::ConnAck(_)))) => return true,
                    Ok(Err(_)) => return false,
                    _ => {}
                }
            }
            false
        };
        assert!(connack(Some(&creds()), "authed"), "the configured login connects");
        assert!(!connack(None, "anon"), "anonymous is refused");
        let wrong = Credentials {
            user: "ops".into(),
            password: "nope".into(),
        };
        assert!(!connack(Some(&wrong), "wrong"), "a wrong password is refused");
    }
}
