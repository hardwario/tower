//! The gateway's MQTT side: the rumqttc **synchronous** client (its `Connection`
//! iterator drives the whole event loop on a plain thread: no async code in this
//! crate; tokio exists only inside these dependencies) and, behind the default-on
//! `embedded-broker` feature, an in-process rumqttd broker so bare `tower gateway`
//! needs zero infrastructure. In embedded mode the gateway's own client loops back
//! over localhost TCP: one MQTT code path regardless of mode.
//!
//! **Access policy** (embedded broker, [`broker_access`]): a loopback bind (the default
//! `127.0.0.1:1883`) is anonymous *by design*, with or without TLS: any local user or process
//! can drive the gateway, exactly like the serial port it owns. A non-loopback bind exposes node
//! keys and the remote shell to the network, so it refuses to start without a login AND TLS
//! (`--broker-tls-identity`); `--broker-allow-plaintext` waives the TLS half explicitly (with a
//! warning). TLS itself lives in `super::tls`.

use std::collections::VecDeque;
// The bind/auth policy only exists where the embedded broker does (and in its tests).
#[cfg(any(feature = "embedded-broker", test))]
use std::collections::HashMap;
#[cfg(any(feature = "embedded-broker", test))]
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::Result;
use rumqttc::{
    Client, Connection, ConnectionError, Event as MqttEvent, Incoming, LastWill, MqttOptions, QoS,
    TlsConfiguration, Transport,
};

use super::engine::Input;
use super::payload;
#[cfg(feature = "embedded-broker")]
use super::tls::BrokerIdentity;
use super::tls::{Target, handshake_hint};
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

/// What the embedded broker's access policy decided for one configuration.
/// Deliberately no `Debug`: the table holds the password.
#[cfg(any(feature = "embedded-broker", test))]
pub(crate) struct BrokerAccess {
    /// rumqttd's static `user → password` table; `None` = anonymous.
    pub auth: Option<HashMap<String, String>>,
    /// A one-line warning to print at startup (plain MQTT allowed off loopback), if any.
    pub warning: Option<String>,
}

/// The embedded broker's access policy (see the module docs), pure so the whole matrix is
/// unit-tested:
///
/// | bind | login | TLS | result |
/// |------|-------|-----|--------|
/// | loopback | none | either | anonymous |
/// | loopback | set | either | login enforced |
/// | other | none | either | refused |
/// | other | set | on | login enforced |
/// | other | set | off | refused, unless `allow_plaintext` (then a warning) |
///
/// Loopback is 127.0.0.0/8 and `::1`, incl. the IPv4-mapped form; "other" includes the
/// wildcards `0.0.0.0` / `::`. A configured login is enforced on any bind.
#[cfg(any(feature = "embedded-broker", test))]
pub(crate) fn broker_access(
    listen: SocketAddr,
    creds: Option<&Credentials>,
    tls: bool,
    allow_plaintext: bool,
) -> std::result::Result<BrokerAccess, String> {
    let auth = creds.map(|c| [(c.user.clone(), c.password.clone())].into_iter().collect());
    if listen.ip().to_canonical().is_loopback() {
        return Ok(BrokerAccess { auth, warning: None });
    }
    let port = listen.port();
    let login = format!("set --mqtt-user and export {PASSWORD_ENV} (clients then need the same login)");
    let serve_tls = format!(
        "serve TLS with --broker-tls-identity <PKCS#12> (its password in {})",
        super::tls::BROKER_PASSWORD_ENV
    );
    let loopback = format!("or bind loopback (--broker 127.0.0.1:{port}) and tunnel (SSH/VPN)");
    let exposed = "a non-loopback bind exposes the gateway (node keys, remote shell) to the network";
    match (creds.is_some(), tls || allow_plaintext) {
        (false, false) => Err(format!(
            "refusing to host the embedded MQTT broker on {listen} without authentication and TLS: \
             {exposed}. To serve it, {login} and {serve_tls}; {loopback}."
        )),
        (false, true) => Err(format!(
            "refusing to host the embedded MQTT broker on {listen} without authentication: {exposed}. \
             To serve it, {login}; {loopback}."
        )),
        (true, false) => Err(format!(
            "refusing to host the embedded MQTT broker on {listen} without TLS: the login and node \
             keys would cross the network in clear text. To serve it, {serve_tls}, or accept clear \
             text explicitly with --broker-allow-plaintext; {loopback}."
        )),
        (true, true) => Ok(BrokerAccess {
            auth,
            warning: (!tls).then(|| {
                format!(
                    "the embedded MQTT broker on {listen} serves plain MQTT (--broker-allow-plaintext): \
                     the login and node keys cross the network in clear text"
                )
            }),
        }),
    }
}

/// Broker connection parameters (resolved from the `tower gateway` flags).
pub(crate) struct MqttParams {
    pub target: Target,
    pub credentials: Option<Credentials>,
    pub client_id: String,
}

/// Client options shared by the gateway and the `nodes`/`net` clients: keep-alive, the
/// packet-size caps (see [`MAX_PAYLOAD`] / [`MAX_INCOMING`]), TLS when the target has a
/// connector, and the login, if any. rumqttc dials `target.host` and hands the same string to
/// the TLS stack as the name the broker's certificate must hold: what the operator typed is
/// what gets verified.
pub(crate) fn options(client_id: String, target: &Target, creds: Option<&Credentials>) -> MqttOptions {
    let mut opts = MqttOptions::new(client_id, target.host.clone(), target.port);
    opts.set_keep_alive(Duration::from_secs(15));
    opts.set_max_packet_size(MAX_INCOMING, MAX_PAYLOAD);
    if let Some(connector) = &target.tls {
        opts.set_transport(Transport::tls_with_config(TlsConfiguration::NativeConnector(
            connector.clone(),
        )));
    }
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
        &params.target,
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
/// password, an untrusted certificate or an unreachable broker is visible instead of failing
/// silently forever. `host` is what the client dials (named in the TLS handshake hint). Runs
/// until the engine hangs up.
pub(crate) fn run(mut connection: Connection, input: Sender<Input>, host: &str) {
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
                let error = match &e {
                    ConnectionError::Tls(_) => format!("{e}{}", handshake_hint(host)),
                    _ => e.to_string(),
                };
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
///
/// With `tls`, the `listen` server speaks MQTT over TLS only, and `local` adds a second, plain
/// listener on loopback for the gateway's own client (see [`embedded_broker`]); both enforce the
/// same login table.
#[cfg(feature = "embedded-broker")]
pub(crate) fn broker_config(
    listen: SocketAddr,
    auth: Option<HashMap<String, String>>,
    tls: Option<&BrokerIdentity>,
    local: Option<SocketAddr>,
) -> rumqttd::Config {
    use rumqttd::{Config, ConnectionSettings, RouterConfig, ServerSettings, TlsConfig};

    let router = RouterConfig {
        max_connections: 128,
        max_outgoing_packet_count: 200,
        max_segment_size: 1024 * 1024,
        max_segment_count: 2,
        ..Default::default()
    };
    let server = |name: &str, listen: SocketAddr, tls: Option<TlsConfig>| ServerSettings {
        name: name.to_string(),
        listen,
        tls,
        next_connection_delay_ms: 1,
        connections: ConnectionSettings {
            // TCP accept → MQTT CONNECT deadline (idle sockets can't pin connection slots).
            connection_timeout_ms: 5000,
            max_payload_size: MAX_PAYLOAD,
            max_inflight_count: 100,
            auth: auth.clone(),
            external_auth: None,
            dynamic_filters: false,
        },
    };
    let tls = tls.map(|id| TlsConfig::NativeTls {
        pkcs12path: id.path.clone(),
        pkcs12pass: id.password.clone(),
    });
    let mut servers = vec![("tower".to_string(), server("tower", listen, tls))];
    if let Some(local) = local {
        servers.push(("tower-local".to_string(), server("tower-local", local, None)));
    }
    Config {
        id: 0,
        router,
        v4: Some(servers.into_iter().collect()),
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

/// Start the embedded rumqttd broker on `listen` under the access policy ([`broker_access`])
/// and return once it is **confirmed listening**, with the address the gateway's own client
/// connects to (plain MQTT). rumqttd reports a failed bind only through `tracing` and then
/// returns `Ok` from `start()`, so the old code announced "embedded MQTT broker on …" and
/// connected to whatever already owned the port. Now: probe-bind first (also the loopback
/// address a wildcard bind is reached through; BSD lets a wildcard bind shadow-coexist with
/// another process's specific one), then wait until the port accepts connections while the
/// broker thread is still alive.
///
/// **With TLS** (`tls`), `listen` speaks MQTT over TLS only, and the gateway's own client does
/// not go through it: it gets a second, plain listener on `127.0.0.1` at a free port, with the
/// same login table. A TLS hop between two threads of one process would authenticate nothing,
/// and it would fail on exactly the certificates operators make: the client would have to dial
/// a name the certificate holds and trust its CA, while a wildcard bind is only reachable as
/// `127.0.0.1` / `localhost`, which a certificate for the gateway's network name does not cover.
/// rumqttd also runs each TLS handshake inside its accept loop, so a peer that stalls one blocks
/// new connections to that listener; the bridge's own (re)connects use the other listener and
/// keep working. The extra listener is a specific loopback bind (it can't be shadowed later), and
/// loopback is the trust boundary the policy already draws: it carries no anonymous access the
/// TLS bind does not have.
#[cfg(feature = "embedded-broker")]
pub(crate) fn embedded_broker(
    listen: SocketAddr,
    creds: Option<&Credentials>,
    tls: Option<&BrokerIdentity>,
    allow_plaintext: bool,
) -> Result<SocketAddr> {
    use std::sync::mpsc;
    use std::time::Instant;

    use anyhow::{Context, bail};

    let access = broker_access(listen, creds, tls.is_some(), allow_plaintext).map_err(anyhow::Error::msg)?;
    let public = connect_addr(listen);
    probe_bind(listen)?;
    if public != listen {
        probe_bind(public)?;
    }
    // The gateway's own way in: the public listener, or (TLS) a plain loopback one. Its port is
    // probed free just before rumqttd binds it; to slip in, another process would have to guess
    // that port and bind it within those microseconds.
    let local = match tls {
        Some(_) => Some(
            std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .and_then(|l| l.local_addr())
                .context("finding a free loopback port for the gateway's own MQTT client")?,
        ),
        None => None,
    };
    let own = local.unwrap_or(public);
    let probes: Vec<SocketAddr> = std::iter::once(public).chain(local).collect();

    let mut broker = rumqttd::Broker::new(broker_config(listen, access.auth, tls, local));
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

    // rumqttd's `start` only returns once every listener's thread has ended, so with two
    // listeners a lost bind shows up as a port that never accepts (the deadline below), not on
    // `died`. A TLS listener accepts the probe's bare TCP connect too (the handshake then fails
    // on EOF and rumqttd moves on).
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(why) = died.try_recv() {
            bail!("embedded MQTT broker on {listen} failed to start: {why}");
        }
        if probes
            .iter()
            .all(|a| std::net::TcpStream::connect_timeout(a, Duration::from_millis(200)).is_ok())
        {
            // rumqttd binds before it accepts, so a lost bind race has already ended its
            // thread by now; give that exit one beat to surface before trusting the port.
            return match died.recv_timeout(Duration::from_millis(100)) {
                Ok(why) => bail!("embedded MQTT broker on {listen} failed to start: {why}"),
                Err(_) => Ok(own),
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

    /// Every cell of the access-policy table (see [`broker_access`]).
    #[test]
    fn broker_access_policy_matrix() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        let table = |acc: &BrokerAccess| acc.auth.as_ref().map(|t| (t.len(), t.get("ops").cloned()));
        let loopback = [
            "127.0.0.1:1883",
            "127.8.9.10:1883",
            "[::1]:1883",
            "[::ffff:127.0.0.1]:1883",
        ];
        let network = ["0.0.0.0:1883", "[::]:1883", "192.168.1.5:1883", "10.0.0.1:2000"];
        let flags = [(false, false), (false, true), (true, false), (true, true)];

        // Loopback: anonymous without a login, the login enforced with one; TLS optional, the
        // plaintext waiver irrelevant, never a warning.
        for l in loopback {
            for (tls, allow) in flags {
                let acc = broker_access(a(l), None, tls, allow).unwrap();
                assert!(acc.auth.is_none() && acc.warning.is_none(), "{l}");
                let acc = broker_access(a(l), Some(&creds()), tls, allow).unwrap();
                assert_eq!(table(&acc), Some((1, Some("s3cret".into()))), "{l}");
                assert!(acc.warning.is_none(), "{l}");
            }
        }

        for l in network {
            // No login: refused whatever the transport, naming the login flags.
            for (tls, allow) in flags {
                let e = broker_access(a(l), None, tls, allow).err().unwrap();
                assert!(e.contains("without authentication"), "{e}");
                assert!(e.contains("--mqtt-user") && e.contains(PASSWORD_ENV), "{e}");
                // Without TLS (and no waiver) the message asks for both.
                assert_eq!(e.contains("--broker-tls-identity"), !tls && !allow, "{e}");
            }
            // Login, no TLS, no waiver: refused, naming both ways out.
            let e = broker_access(a(l), Some(&creds()), false, false).err().unwrap();
            assert!(e.contains("without TLS") && e.contains("clear text"), "{e}");
            assert!(
                e.contains("--broker-tls-identity") && e.contains("--broker-allow-plaintext"),
                "{e}"
            );
            assert!(e.contains(crate::gateway::tls::BROKER_PASSWORD_ENV), "{e}");
            // Login + TLS: served, no warning.
            let acc = broker_access(a(l), Some(&creds()), true, false).unwrap();
            assert_eq!(table(&acc), Some((1, Some("s3cret".into()))));
            assert!(acc.warning.is_none());
            // Login + explicit waiver: served, with the one-line clear-text warning.
            let acc = broker_access(a(l), Some(&creds()), false, true).unwrap();
            assert_eq!(table(&acc), Some((1, Some("s3cret".into()))));
            let w = acc.warning.expect("plain MQTT off loopback is announced");
            assert!(
                w.contains("clear text") && w.contains("--broker-allow-plaintext"),
                "{w}"
            );
            assert!(!w.contains('\n'), "one line: {w}");
        }

        // No message ever carries the password.
        for l in network {
            for (tls, allow) in flags {
                if let Err(e) = broker_access(a(l), Some(&creds()), tls, allow) {
                    assert!(!e.contains("s3cret"), "{e}");
                }
            }
        }
    }

    #[cfg(feature = "embedded-broker")]
    #[test]
    fn broker_config_is_bounded_and_carries_auth() {
        let listen: SocketAddr = "0.0.0.0:1883".parse().unwrap();
        let auth = broker_access(listen, Some(&creds()), false, true).unwrap().auth;
        let cfg = broker_config(listen, auth, None, None);
        assert_eq!(cfg.router.max_segment_size, 1024 * 1024);
        assert_eq!(cfg.router.max_segment_count, 2);
        let servers = cfg.v4.as_ref().unwrap();
        assert_eq!(servers.len(), 1, "plain MQTT: one listener");
        let server = &servers["tower"];
        assert_eq!(server.listen, listen);
        assert!(server.tls.is_none());
        assert_eq!(server.connections.max_payload_size, MAX_PAYLOAD);
        let table = server.connections.auth.as_ref().expect("auth enabled");
        assert_eq!(table.get("ops").map(String::as_str), Some("s3cret"));
        // Loopback without credentials: no auth table.
        let lo: SocketAddr = "127.0.0.1:1883".parse().unwrap();
        let cfg = broker_config(
            lo,
            broker_access(lo, None, false, false).unwrap().auth,
            None,
            None,
        );
        assert!(cfg.v4.unwrap()["tower"].connections.auth.is_none());
    }

    #[cfg(feature = "embedded-broker")]
    #[test]
    fn broker_config_tls_adds_a_plain_loopback_listener_with_the_same_login() {
        let listen: SocketAddr = "0.0.0.0:8883".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:40001".parse().unwrap();
        let id = BrokerIdentity {
            path: "/etc/tower/broker.p12".into(),
            password: "p12-pass".into(),
        };
        let auth = broker_access(listen, Some(&creds()), true, false).unwrap().auth;
        let cfg = broker_config(listen, auth, Some(&id), Some(local));
        let servers = cfg.v4.unwrap();
        assert_eq!(servers.len(), 2);
        let public = &servers["tower"];
        assert_eq!(public.listen, listen);
        match &public.tls {
            Some(rumqttd::TlsConfig::NativeTls {
                pkcs12path,
                pkcs12pass,
            }) => assert_eq!(
                (pkcs12path.as_str(), pkcs12pass.as_str()),
                (id.path.as_str(), "p12-pass")
            ),
            _ => panic!("the public listener must be native-tls"),
        }
        let own = &servers["tower-local"];
        assert_eq!(own.listen, local);
        assert!(own.tls.is_none());
        assert_eq!(
            own.connections
                .auth
                .as_ref()
                .and_then(|t| t.get("ops"))
                .map(String::as_str),
            Some("s3cret"),
            "the loopback listener enforces the same login"
        );
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
        let own = embedded_broker(addr, None, None, false).expect("broker confirmed listening");
        assert_eq!(
            own, addr,
            "plain MQTT: the gateway's client uses the one listener"
        );

        let opts = options("test-client".into(), &plain("127.0.0.1", port), None);
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
        let e = embedded_broker(addr, None, None, false).unwrap_err().to_string();
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
        embedded_broker(addr, Some(&creds()), None, false).expect("broker up");

        let connack = |c: Option<&Credentials>, id: &str| -> bool {
            connect_once(options(id.into(), &plain("127.0.0.1", port), c), WAIT).is_ok()
        };
        assert!(connack(Some(&creds()), "authed"), "the configured login connects");
        assert!(!connack(None, "anon"), "anonymous is refused");
        let wrong = Credentials {
            user: "ops".into(),
            password: "nope".into(),
        };
        assert!(!connack(Some(&wrong), "wrong"), "a wrong password is refused");
    }

    #[cfg(feature = "embedded-broker")]
    fn plain(host: &str, port: u16) -> Target {
        Target {
            host: host.into(),
            port,
            tls: None,
        }
    }

    /// A free TCP port on loopback (a fixed one would collide across parallel CI runs).
    #[cfg(feature = "embedded-broker")]
    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    /// How long a test client waits for a CONNACK that should come (generous for slow CI).
    #[cfg(feature = "embedded-broker")]
    const WAIT: Duration = Duration::from_secs(10);

    /// Connect one client and wait up to `wait` for its CONNACK: the live session, or the first
    /// connection error (`None`: no answer in time).
    #[cfg(feature = "embedded-broker")]
    fn connect_once(
        opts: MqttOptions,
        wait: Duration,
    ) -> std::result::Result<(Client, Connection), Option<Box<ConnectionError>>> {
        let (client, mut conn) = Client::new(opts, 10);
        let deadline = std::time::Instant::now() + wait;
        while std::time::Instant::now() < deadline {
            match conn.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(MqttEvent::Incoming(Incoming::ConnAck(_)))) => return Ok((client, conn)),
                Ok(Err(e)) => return Err(Some(Box::new(e))),
                _ => {}
            }
        }
        Err(None)
    }

    /// A retained publish comes back through a subscription on the same session.
    #[cfg(feature = "embedded-broker")]
    fn round_trip(client: &Client, conn: &mut Connection, topic: &str) -> bool {
        client.subscribe(topic, QoS::AtLeastOnce).unwrap();
        client
            .publish(topic, QoS::AtLeastOnce, true, b"hi".as_slice())
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Ok(Ok(MqttEvent::Incoming(Incoming::Publish(p)))) =
                conn.recv_timeout(Duration::from_millis(500))
                && p.topic == topic
            {
                return p.payload.as_ref() == b"hi";
            }
        }
        false
    }

    /// MQTT over TLS end to end through the real embedded broker, against a throwaway CA and a
    /// server identity for `localhost` + `127.0.0.1` minted for this test (`tls::test_pki`), on
    /// ephemeral loopback ports. Covers what an operator can get wrong: a trusted CA connects
    /// (by name and by IP) and carries data; no CA (OS roots only) is an untrusted certificate;
    /// a certificate for another name is a hostname mismatch; plain MQTT on the TLS port gets no
    /// session. Also: the gateway's own client is handed the plain loopback listener.
    #[cfg(feature = "embedded-broker")]
    #[test]
    fn embedded_broker_tls_round_trip() {
        use crate::gateway::tls::{self, ClientTlsFlags, test_pki};

        let pki = test_pki::make();
        let tls_target = |spec: &str, ca: bool, identity: bool| -> Target {
            let flags = ClientTlsFlags {
                tls: false,
                ca: ca.then_some(pki.ca_pem.as_path()),
                identity: identity.then_some(pki.client_p12.as_path()),
            };
            tls::resolve_target_with(spec, &flags, test_pki::PASSWORD).unwrap()
        };

        let id = tls::broker_identity_with(&pki.server_p12, test_pki::PASSWORD.into()).unwrap();
        let port = free_port();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let own = embedded_broker(addr, None, Some(&id), false).expect("TLS broker confirmed listening");
        assert!(
            own.ip().is_loopback() && own != addr,
            "own client: a separate loopback listener"
        );

        // Trusted CA: by DNS name and by IP address (both are in the certificate), with and
        // without a client identity (rumqttd's native-tls acceptor never asks for one, so this
        // proves the identity loads and the handshake still completes).
        for (i, (host, identity)) in [("localhost", false), ("127.0.0.1", false), ("localhost", true)]
            .into_iter()
            .enumerate()
        {
            let spec = format!("mqtts://{host}:{port}");
            let (client, mut conn) = connect_once(
                options("tls-ok".into(), &tls_target(&spec, true, identity), None),
                WAIT,
            )
            .unwrap_or_else(|e| panic!("{spec} (identity {identity}): {e:?}"));
            assert!(
                round_trip(&client, &mut conn, &format!("tower/tls/{i}")),
                "{spec}: data over TLS"
            );
        }

        // No --mqtt-ca: the OS roots don't know the test CA.
        let spec = format!("mqtts://localhost:{port}");
        match connect_once(
            options("tls-untrusted".into(), &tls_target(&spec, false, false), None),
            WAIT,
        ) {
            Err(Some(e)) if matches!(*e, ConnectionError::Tls(_)) => eprintln!("untrusted, as expected: {e}"),
            Ok(_) => panic!("a certificate from an unknown CA must be refused"),
            Err(e) => panic!("expected a TLS error for an untrusted certificate, got {e:?}"),
        }

        // The gateway's own way in: plain MQTT on the loopback listener. It keeps working while a
        // peer holds the TLS listener's accept loop with a handshake it never finishes (rumqttd
        // runs each TLS handshake inline there), which is why the bridge does not use that port.
        let staller = std::net::TcpStream::connect(addr).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let (client, mut conn) =
            connect_once(options("own".into(), &plain("127.0.0.1", own.port()), None), WAIT)
                .expect("the gateway's own listener takes plain MQTT, even with the TLS port stalled");
        assert!(round_trip(&client, &mut conn, "tower/own"));
        drop(staller);

        // Plain MQTT against the TLS port gets no session (OpenSSL rejects the first byte; on
        // macOS the broker waits for a TLS record that never comes, hence the short deadline).
        assert!(
            connect_once(
                options("plain".into(), &plain("127.0.0.1", port), None),
                Duration::from_secs(3)
            )
            .is_err()
        );

        // A trusted CA but a certificate for another name: the hostname check stays on.
        let other = tls::broker_identity_with(&pki.other_p12, test_pki::PASSWORD.into()).unwrap();
        let port2 = free_port();
        embedded_broker(
            SocketAddr::from((Ipv4Addr::LOCALHOST, port2)),
            None,
            Some(&other),
            false,
        )
        .expect("second TLS broker up");
        let spec = format!("mqtts://localhost:{port2}");
        match connect_once(
            options("tls-mismatch".into(), &tls_target(&spec, true, false), None),
            WAIT,
        ) {
            Err(Some(e)) if matches!(*e, ConnectionError::Tls(_)) => {
                eprintln!("name mismatch, as expected: {e}")
            }
            Ok(_) => panic!("a certificate for tower.invalid must not pass for localhost"),
            Err(e) => panic!("expected a TLS error for a hostname mismatch, got {e:?}"),
        }
    }

    /// A TLS embedded broker keeps the login: the plain loopback listener for the gateway's own
    /// client enforces it too (it is not an anonymous side door).
    #[cfg(feature = "embedded-broker")]
    #[test]
    fn embedded_broker_tls_own_listener_enforces_the_login() {
        use crate::gateway::tls::{self, test_pki};

        let pki = test_pki::make();
        let id = tls::broker_identity_with(&pki.server_p12, test_pki::PASSWORD.into()).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, free_port()));
        let own = embedded_broker(addr, Some(&creds()), Some(&id), false).expect("broker up");
        let target = plain("127.0.0.1", own.port());
        assert!(connect_once(options("own-anon".into(), &target, None), WAIT).is_err());
        assert!(connect_once(options("own-authed".into(), &target, Some(&creds())), WAIT).is_ok());
    }
}
