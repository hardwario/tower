//! MQTT over TLS: where a client connects (`--mqtt [mqtts://]HOST[:PORT]`), the client's TLS
//! connector (`tower nodes` / `tower net`, and `tower gateway --mqtt` toward an external broker),
//! and the embedded broker's server identity (`tower gateway --broker-tls-identity`).
//!
//! Every TLS stack here is the platform's own, through native-tls: Security.framework on macOS,
//! SChannel on Windows, OpenSSL on Linux (built from source and linked statically, so a release
//! binary stays self-contained). rustls is deliberately absent: the rustls that rumqttc 0.25 /
//! rumqttd 0.20 build against (0.22/0.23 with rustls-webpki 0.102) carries RustSec advisories,
//! and `cargo audit` gates CI.
//!
//! The rules, in one place:
//! - a client trusts the OS roots plus any `--mqtt-ca` PEM roots, and ALWAYS checks that the
//!   broker's certificate names the host it dials. There is no "insecure" switch: a self-signed
//!   broker is trusted by handing its CA to `--mqtt-ca`, not by turning the checks off;
//! - the PKCS#12 passwords come only from the environment (argv is visible in `ps`) and are never
//!   printed;
//! - every file is read and validated at startup, so a wrong path or password fails before any
//!   serial port is opened, with the flag's name in the message.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use native_tls::{Certificate, Identity, TlsConnector};

/// The MQTT port without and with TLS (IANA: 1883 `mqtt`, 8883 `secure-mqtt`).
pub(crate) const MQTT_PORT: u16 = 1883;
pub(crate) const MQTTS_PORT: u16 = 8883;

/// Env var for the password of `--mqtt-client-identity` (PKCS#12).
pub(crate) const CLIENT_IDENTITY_PASSWORD_ENV: &str = "TOWER_MQTT_CLIENT_IDENTITY_PASSWORD";

/// Env var for the password of `--broker-tls-identity` (PKCS#12).
#[cfg(any(feature = "embedded-broker", test))]
pub(crate) const BROKER_PASSWORD_ENV: &str = "TOWER_BROKER_TLS_PASSWORD";

/// A parsed `--mqtt` value: host, port and whether the link is TLS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

/// Parse `--mqtt`: `HOST`, `HOST:PORT`, `mqtt://HOST[:PORT]` (plain) or `mqtts://HOST[:PORT]`
/// (TLS). `tls_flag` (`--mqtt-tls`) makes a scheme-less address TLS too. The default port follows
/// the transport: 1883 plain, 8883 TLS. IPv6 literals take the bracketed form (`[::1]:8883`); a
/// bare one (`::1`) is all host. Strict on purpose: a typo in the port is an error, not a silent
/// fallback to the default.
pub(crate) fn parse_endpoint(spec: &str, tls_flag: bool) -> std::result::Result<Endpoint, String> {
    let (tls, rest) = match spec.split_once("://") {
        Some((scheme, rest)) => match scheme.to_ascii_lowercase().as_str() {
            "mqtts" => (true, rest),
            "mqtt" if tls_flag => {
                return Err(format!(
                    "--mqtt {spec} is plain MQTT (mqtt://) but --mqtt-tls asks for TLS; use mqtts://"
                ));
            }
            "mqtt" => (false, rest),
            other => {
                return Err(format!(
                    "--mqtt: unsupported scheme \"{other}://\" (use mqtt:// or mqtts://)"
                ));
            }
        },
        None => (tls_flag, spec),
    };
    // Never echo an address that holds a login: it may carry a password.
    if rest.contains('@') {
        let fix = "use --mqtt-user and the TOWER_MQTT_PASSWORD env var";
        return Err(format!("--mqtt: the address must not hold a login; {fix}"));
    }
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.contains('/') {
        return Err(format!(
            "--mqtt {spec}: a broker address is [mqtts://]HOST[:PORT], without a path"
        ));
    }
    let default_port = if tls { MQTTS_PORT } else { MQTT_PORT };
    let (host, port) = split_host_port(rest, default_port).map_err(|e| format!("--mqtt {spec}: {e}"))?;
    if host.is_empty() {
        return Err(format!("--mqtt {spec}: no host"));
    }
    Ok(Endpoint { host, port, tls })
}

/// Split `HOST[:PORT]` (see [`parse_endpoint`]).
fn split_host_port(s: &str, default_port: u16) -> std::result::Result<(String, u16), String> {
    if let Some(rest) = s.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or("an IPv6 address needs its closing ']'")?;
        let port = match tail {
            "" => default_port,
            t => parse_port(t.strip_prefix(':').ok_or("expected :PORT after ']'")?)?,
        };
        return Ok((host.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Ok((host.to_string(), parse_port(port)?)),
        _ => Ok((s.to_string(), default_port)),
    }
}

fn parse_port(p: &str) -> std::result::Result<u16, String> {
    p.parse::<u16>()
        .ok()
        .filter(|&n| n != 0)
        .ok_or_else(|| format!("invalid port \"{p}\" (1-65535)"))
}

/// Where and how an MQTT client connects: the host it dials (and that the broker's certificate
/// must name), the port, and the TLS connector (`None` = plain MQTT).
#[derive(Clone)]
pub(crate) struct Target {
    pub host: String,
    pub port: u16,
    pub tls: Option<TlsConnector>,
}

impl Target {
    /// The address as an operator would type it back (`mqtts://host:8883`, `[::1]:1883`).
    pub(crate) fn url(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let scheme = if self.tls.is_some() { "mqtts://" } else { "" };
        format!("{scheme}{host}:{}", self.port)
    }
}

/// The client-side TLS flags, shared by `tower gateway --mqtt` and the client commands.
pub(crate) struct ClientTlsFlags<'a> {
    /// `--mqtt-tls`.
    pub tls: bool,
    /// `--mqtt-ca`: extra trusted roots (PEM).
    pub ca: Option<&'a Path>,
    /// `--mqtt-client-identity`: a certificate + key for mutual TLS (PKCS#12).
    pub identity: Option<&'a Path>,
}

/// Resolve `--mqtt` and the TLS flags into a [`Target`], reading and validating the CA and
/// identity files now (the identity's password from [`CLIENT_IDENTITY_PASSWORD_ENV`]).
pub(crate) fn resolve_target(spec: &str, flags: &ClientTlsFlags) -> Result<Target> {
    let password = match flags.identity {
        Some(_) => env_password(CLIENT_IDENTITY_PASSWORD_ENV)?,
        None => String::new(),
    };
    resolve_target_with(spec, flags, &password)
}

/// [`resolve_target`] with the identity password passed in (the tests' entry point: they must not
/// touch the process environment).
pub(crate) fn resolve_target_with(
    spec: &str,
    flags: &ClientTlsFlags,
    identity_password: &str,
) -> Result<Target> {
    let ep = parse_endpoint(spec, flags.tls).map_err(anyhow::Error::msg)?;
    if !ep.tls && (flags.ca.is_some() || flags.identity.is_some()) {
        bail!(
            "--mqtt-ca and --mqtt-client-identity apply to a TLS link: use --mqtt mqtts://HOST[:PORT] \
             or add --mqtt-tls"
        );
    }
    let tls = if ep.tls {
        Some(client_connector(flags.ca, flags.identity, identity_password)?)
    } else {
        None
    };
    Ok(Target {
        host: ep.host,
        port: ep.port,
        tls,
    })
}

/// The client's TLS connector: the OS roots plus every certificate in `ca` (PEM), and the client
/// identity for mutual TLS if one is given. native-tls's defaults stand: TLS 1.2 at least, chain
/// AND hostname verification on.
fn client_connector(
    ca: Option<&Path>,
    identity: Option<&Path>,
    identity_password: &str,
) -> Result<TlsConnector> {
    let mut builder = TlsConnector::builder();
    if let Some(path) = ca {
        for cert in read_ca(path)? {
            builder.add_root_certificate(cert);
        }
    }
    if let Some(path) = identity {
        let id = load_identity(path, identity_password, CLIENT_IDENTITY_PASSWORD_ENV)
            .with_context(|| format!("--mqtt-client-identity {}", path.display()))?;
        builder.identity(id);
    }
    builder.build().context("setting up the MQTT TLS client")
}

/// Read `--mqtt-ca`: every `CERTIFICATE` block of a PEM file (a bundle may hold several roots).
fn read_ca(path: &Path) -> Result<Vec<Certificate>> {
    let what = || format!("--mqtt-ca {}", path.display());
    let pem = std::fs::read(path).with_context(what)?;
    let blocks = pem_certificates(&pem);
    if blocks.is_empty() {
        bail!(
            "{}: no PEM certificate (-----BEGIN CERTIFICATE-----) in the file",
            what()
        );
    }
    blocks
        .iter()
        .enumerate()
        .map(|(i, block)| {
            Certificate::from_pem(block.as_bytes())
                .with_context(|| format!("{}: certificate #{} is not valid", what(), i + 1))
        })
        .collect()
}

/// The `CERTIFICATE` blocks of a PEM file, each with its BEGIN/END lines (anything else, e.g. a
/// comment or a key, is skipped). native-tls parses one certificate per call.
fn pem_certificates(pem: &[u8]) -> Vec<String> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let text = String::from_utf8_lossy(pem);
    let mut rest = &text[..];
    let mut out = Vec::new();
    while let Some(start) = rest.find(BEGIN) {
        let Some(len) = rest[start..].find(END) else {
            break;
        };
        let end = start + len + END.len();
        out.push(format!("{}\n", &rest[start..end]));
        rest = &rest[end..];
    }
    out
}

/// A PKCS#12 password from the environment (unset = empty, which a password-less file needs).
fn env_password(var: &str) -> Result<String> {
    match std::env::var(var) {
        Ok(p) => Ok(p),
        Err(std::env::VarError::NotPresent) => Ok(String::new()),
        Err(std::env::VarError::NotUnicode(_)) => bail!("{var} is not valid UTF-8"),
    }
}

/// Load a PKCS#12 identity (certificate chain + private key). `password_env` only names the
/// variable in the error; the password itself is never part of a message.
fn load_identity(path: &Path, password: &str, password_env: &str) -> Result<Identity> {
    let der = std::fs::read(path)?;
    pkcs12_identity(&der, password).map_err(|e| {
        anyhow!(
            "not a usable PKCS#12 identity (a certificate with its private key), or the password in \
             {password_env} is wrong: {e}"
        )
    })
}

/// `Identity::from_pkcs12`, minus a panic: native-tls's macOS backend panics ("Pkcs12 files must
/// include an identity") on a PKCS#12 that holds no private key, e.g. a certificate exported on
/// its own, where the other backends return an error. Catch it and report it like any other
/// unusable file. On Apple targets the panic hook is muted for the call, so the operator sees one
/// error line instead of a panic report. The hook is process-wide: this runs at startup, before
/// the gateway or a client command starts any thread (in the test suite a concurrent test's
/// panic inside that window would lose only its message, not its failure).
fn pkcs12_identity(der: &[u8], password: &str) -> std::result::Result<Identity, String> {
    #[cfg(target_vendor = "apple")]
    let hook = {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        hook
    };
    let result = std::panic::catch_unwind(|| Identity::from_pkcs12(der, password));
    #[cfg(target_vendor = "apple")]
    std::panic::set_hook(hook);
    match result {
        Ok(Ok(id)) => Ok(id),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("the file holds no private key".into()),
    }
}

/// The embedded broker's TLS server identity, loaded: what its TLS front end
/// (`super::tls_front`) serves. The file is read once, at startup, and the password is not kept:
/// the file may move afterwards, and a new certificate takes a restart (a per-connection reload
/// would let every unauthenticated peer trigger a PKCS#12 decryption, and on Windows a key
/// import).
#[cfg(feature = "embedded-broker")]
pub(crate) struct BrokerIdentity {
    pub acceptor: native_tls::TlsAcceptor,
}

/// Load and validate `--broker-tls-identity` (password from [`BROKER_PASSWORD_ENV`]).
#[cfg(feature = "embedded-broker")]
pub(crate) fn broker_identity(path: &Path) -> Result<BrokerIdentity> {
    broker_identity_with(path, &env_password(BROKER_PASSWORD_ENV)?)
}

/// [`broker_identity`] with the password passed in (the tests' entry point). Validated the way
/// the broker uses it: as a TLS server identity, not just a parseable file. native-tls's
/// defaults stand: TLS 1.2 at least, no client certificate requested.
#[cfg(feature = "embedded-broker")]
pub(crate) fn broker_identity_with(path: &Path, password: &str) -> Result<BrokerIdentity> {
    let what = || format!("--broker-tls-identity {}", path.display());
    let id = load_identity(path, password, BROKER_PASSWORD_ENV).with_context(what)?;
    let acceptor = native_tls::TlsAcceptor::new(id)
        .with_context(|| format!("{}: not usable as a TLS server identity", what()))?;
    Ok(BrokerIdentity { acceptor })
}

/// Is `host` (as given to `--mqtt`) this machine: `localhost` or a loopback IP literal? Names are
/// not resolved: anything else counts as the network.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// The one-line warning for a login sent over plain MQTT to another machine. Not refused: the
/// broker may sit on a trusted segment; but the operator should know the password (and any node
/// key a command adds or reveals) crosses the network in clear text.
pub(crate) fn cleartext_login_warning(target: &Target, have_login: bool) -> Option<String> {
    (have_login && target.tls.is_none() && !is_loopback_host(&target.host)).then(|| {
        format!(
            "{} is plain MQTT: the login (and any node key a command adds or reveals) crosses the \
             network in clear text; use mqtts:// or --mqtt-tls",
            target.url()
        )
    })
}

/// Appended to a failed or silent plain-MQTT connect: a TLS-only port closes on the first
/// non-TLS byte, or (some TLS stacks) waits for a whole record that never comes.
pub(crate) const PLAIN_TO_TLS_HINT: &str = "if the port speaks TLS, connect with mqtts://";

/// What to check after a failed TLS handshake (`ConnectionError::Tls`). The platform stacks word
/// the cause differently ("certificate was not trusted", "certificate verify failed", "host name
/// mismatch", a bare "handshake failure" when the broker wanted a client certificate), so the
/// hint names everything a handshake needs from this side.
pub(crate) fn handshake_hint(host: &str) -> String {
    format!(
        " (the broker's certificate must chain to a trusted root, the OS roots or --mqtt-ca <PEM>, \
         and name \"{host}\", the host --mqtt dials; the port must speak TLS; a broker that \
         requires client certificates needs --mqtt-client-identity)"
    )
}

/// A throwaway PKI for the TLS tests, minted at test time so nothing in the tree can expire: a CA
/// and identities signed by it, valid from yesterday for 30 days (macOS refuses any TLS server
/// certificate valid for more than 825 days, custom roots included).
#[cfg(test)]
pub(crate) mod test_pki {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use chrono::{Datelike, Duration, Utc};
    use p12_keystore::{KeyStore, KeyStoreEntry, PrivateKeyChain};
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
        KeyUsagePurpose, date_time_ymd,
    };

    /// The password of every PKCS#12 file here.
    pub(crate) const PASSWORD: &str = "test-only";

    /// The files, in a directory of their own that is removed on drop.
    pub(crate) struct Pki {
        dir: PathBuf,
        /// The CA certificate (PEM), what `--mqtt-ca` takes.
        pub ca_pem: PathBuf,
        /// Server identity for `localhost` and `127.0.0.1`.
        pub server_p12: PathBuf,
        /// Server identity for `tower.invalid` only (a hostname mismatch on localhost).
        pub other_p12: PathBuf,
        /// Client identity (mutual TLS).
        pub client_p12: PathBuf,
        /// A PKCS#12 holding only a certificate, no key.
        pub cert_only_p12: PathBuf,
    }

    impl Drop for Pki {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn validity(params: &mut CertificateParams) {
        let day = |d: chrono::DateTime<Utc>| date_time_ymd(d.year(), d.month() as u8, d.day() as u8);
        params.not_before = day(Utc::now() - Duration::days(1));
        params.not_after = day(Utc::now() + Duration::days(30));
    }

    /// `name` is the alias and the localKeyId, unique per file: Windows persists every imported
    /// key under the user's key store, and tests running in parallel with the same name replace
    /// each other's key, so the certificate no longer matches it ("No identity found").
    fn p12(name: &str, chain: &[&rcgen::Certificate], key: &KeyPair, with_key: bool) -> Vec<u8> {
        let certs: Vec<_> = chain
            .iter()
            .map(|c| p12_keystore::Certificate::from_der(c.der()).unwrap())
            .collect();
        let mut store = KeyStore::new();
        if with_key {
            let key = p12_keystore::PrivateKey::from_der(&key.serialize_der()).unwrap();
            let entry = PrivateKeyChain::new(name.as_bytes(), key, certs);
            store.add_entry(name, KeyStoreEntry::PrivateKeyChain(entry));
        } else {
            store.add_entry(name, KeyStoreEntry::Certificate(certs[0].clone()));
        }
        store.writer(PASSWORD).write().unwrap()
    }

    pub(crate) fn make() -> Pki {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tower-tls-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| -> PathBuf { Path::new(&dir).join(name) };

        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.distinguished_name.push(DnType::CommonName, "tower test CA");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        validity(&mut ca);
        let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();

        let leaf = |names: &[&str], eku: ExtendedKeyUsagePurpose| {
            let mut p =
                CertificateParams::new(names.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
            p.distinguished_name.push(DnType::CommonName, names[0]);
            p.is_ca = IsCa::ExplicitNoCa;
            p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            p.extended_key_usages = vec![eku];
            validity(&mut p);
            let key = KeyPair::generate().unwrap();
            let cert = p.signed_by(&key, &ca).unwrap();
            (cert, key)
        };
        let (server, server_key) = leaf(&["localhost", "127.0.0.1"], ExtendedKeyUsagePurpose::ServerAuth);
        let (other, other_key) = leaf(&["tower.invalid"], ExtendedKeyUsagePurpose::ServerAuth);
        let (client, client_key) = leaf(&["tower-test-client"], ExtendedKeyUsagePurpose::ClientAuth);

        let pki = Pki {
            ca_pem: path("ca.pem"),
            server_p12: path("server.p12"),
            other_p12: path("other.p12"),
            client_p12: path("client.p12"),
            cert_only_p12: path("cert-only.p12"),
            dir: dir.clone(),
        };
        let ca_cert: &rcgen::Certificate = ca.as_ref();
        std::fs::write(&pki.ca_pem, ca.pem()).unwrap();
        let id = dir.file_name().unwrap().to_string_lossy().into_owned();
        let name = |file: &str| format!("{id}-{file}");
        std::fs::write(
            &pki.server_p12,
            p12(&name("server"), &[&server, ca_cert], &server_key, true),
        )
        .unwrap();
        std::fs::write(
            &pki.other_p12,
            p12(&name("other"), &[&other, ca_cert], &other_key, true),
        )
        .unwrap();
        std::fs::write(
            &pki.client_p12,
            p12(&name("client"), &[&client, ca_cert], &client_key, true),
        )
        .unwrap();
        std::fs::write(
            &pki.cert_only_p12,
            p12(&name("cert-only"), &[&server], &server_key, false),
        )
        .unwrap();
        pki
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(host: &str, port: u16, tls: bool) -> Endpoint {
        Endpoint {
            host: host.into(),
            port,
            tls,
        }
    }

    #[test]
    fn endpoint_forms_and_default_ports() {
        let p = |s: &str, tls: bool| parse_endpoint(s, tls).unwrap();
        assert_eq!(p("host", false), ep("host", 1883, false));
        assert_eq!(p("host:1884", false), ep("host", 1884, false));
        assert_eq!(p("mqtt://host", false), ep("host", 1883, false));
        assert_eq!(p("mqtts://host", false), ep("host", 8883, true));
        assert_eq!(p("MQTTS://host:9000/", false), ep("host", 9000, true));
        // --mqtt-tls: a scheme-less address is TLS and defaults to 8883.
        assert_eq!(p("host", true), ep("host", 8883, true));
        assert_eq!(p("host:1883", true), ep("host", 1883, true));
        assert_eq!(p("mqtts://host", true), ep("host", 8883, true));
        // IPv6: bracketed with or without a port; a bare literal is all host.
        assert_eq!(p("[::1]:1884", false), ep("::1", 1884, false));
        assert_eq!(p("[::1]", false), ep("::1", 1883, false));
        assert_eq!(p("mqtts://[::1]", false), ep("::1", 8883, true));
        assert_eq!(p("::1", false), ep("::1", 1883, false), "bare v6 is all host");
    }

    #[test]
    fn endpoint_errors_are_explicit() {
        let e = |s: &str, tls: bool| parse_endpoint(s, tls).unwrap_err();
        assert!(e("mqtt://host", true).contains("--mqtt-tls"));
        assert!(e("ws://host", false).contains("unsupported scheme"));
        assert!(e("host:99999", false).contains("invalid port"));
        assert!(e("host:0", false).contains("invalid port"));
        assert!(e("host:abc", false).contains("invalid port"));
        assert!(e("[::1", false).contains("']'"));
        assert!(e("mqtts://", false).contains("no host"));
        assert!(e("mqtts://host/tower", false).contains("without a path"));
        // A login in the URL is refused without echoing it.
        let m = e("mqtts://ops:s3cret@host", false);
        assert!(m.contains("--mqtt-user") && !m.contains("s3cret"), "{m}");
    }

    #[test]
    fn target_url_round_trips() {
        let t = |host: &str, tls: bool| Target {
            host: host.into(),
            port: 8883,
            tls: tls.then(|| TlsConnector::new().unwrap()),
        };
        assert_eq!(t("broker.lan", true).url(), "mqtts://broker.lan:8883");
        assert_eq!(t("::1", false).url(), "[::1]:8883");
    }

    #[test]
    fn loopback_hosts() {
        for h in [
            "localhost",
            "LOCALHOST",
            "localhost.",
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in [
            "broker.lan",
            "192.168.1.5",
            "0.0.0.0",
            "::",
            "localhost.example.com",
        ] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    #[test]
    fn cleartext_login_warning_matrix() {
        let target = |host: &str, tls: bool| Target {
            host: host.into(),
            port: 1883,
            tls: tls.then(|| TlsConnector::new().unwrap()),
        };
        // Warned: a login over plain MQTT to another machine.
        let w = cleartext_login_warning(&target("broker.lan", false), true).unwrap();
        assert!(
            w.contains("clear text") && w.contains("mqtts://") && !w.contains('\n'),
            "{w}"
        );
        // Not warned: TLS, no login, or loopback.
        assert!(cleartext_login_warning(&target("broker.lan", true), true).is_none());
        assert!(cleartext_login_warning(&target("broker.lan", false), false).is_none());
        assert!(cleartext_login_warning(&target("localhost", false), true).is_none());
        assert!(cleartext_login_warning(&target("127.0.0.1", false), true).is_none());
    }

    #[test]
    fn pem_bundle_splits_into_certificates() {
        let one = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";
        let bundle = format!("# root A\n{one}\nnoise\n{one}\n-----BEGIN PRIVATE KEY-----\nBBBB\n");
        let blocks = pem_certificates(bundle.as_bytes());
        assert_eq!(blocks.len(), 2);
        assert!(
            blocks
                .iter()
                .all(|b| b.starts_with("-----BEGIN") && b.ends_with("-----\n"))
        );
        assert!(pem_certificates(b"\x30\x82\x01\x00 DER bytes").is_empty());
    }

    fn flags<'a>(tls: bool, ca: Option<&'a Path>, identity: Option<&'a Path>) -> ClientTlsFlags<'a> {
        ClientTlsFlags { tls, ca, identity }
    }

    #[test]
    fn client_tls_config_construction() {
        let pki = test_pki::make();
        let ca = Some(pki.ca_pem.as_path());
        let id = Some(pki.client_p12.as_path());

        // Plain MQTT: no connector, and the TLS-only flags are refused rather than ignored.
        let t = resolve_target_with("broker.lan", &flags(false, None, None), "").unwrap();
        assert!(t.tls.is_none());
        let e = resolve_target_with("broker.lan", &flags(false, ca, None), "")
            .err()
            .unwrap();
        assert!(e.to_string().contains("mqtts://"), "{e}");
        assert!(resolve_target_with("mqtt://h", &flags(false, None, id), test_pki::PASSWORD).is_err());

        // TLS with the OS roots only, with an extra CA, and with a client identity.
        let t = resolve_target_with("mqtts://broker.lan", &flags(false, None, None), "").unwrap();
        assert!(t.tls.is_some() && t.port == MQTTS_PORT);
        assert!(resolve_target_with("broker.lan", &flags(true, ca, None), "").is_ok());
        assert!(resolve_target_with("broker.lan", &flags(true, ca, id), test_pki::PASSWORD).is_ok());
    }

    #[test]
    fn unusable_files_fail_with_the_flag_named() {
        let pki = test_pki::make();
        let err = |spec: &str, f: ClientTlsFlags, pw: &str| -> String {
            format!(
                "{:#}",
                resolve_target_with(spec, &f, pw).err().expect("must fail")
            )
        };
        let missing = pki.ca_pem.with_file_name("missing.pem");
        let m = err("mqtts://h", flags(false, Some(&missing), None), "");
        assert!(m.contains("--mqtt-ca") && m.contains("missing.pem"), "{m}");
        // A PKCS#12 is not a PEM CA.
        let m = err("mqtts://h", flags(false, Some(&pki.server_p12), None), "");
        assert!(m.contains("no PEM certificate"), "{m}");
        // Wrong password: named by its env var, never echoed.
        let m = err(
            "mqtts://h",
            flags(false, None, Some(&pki.client_p12)),
            "wrong-pass",
        );
        assert!(
            m.contains("--mqtt-client-identity") && m.contains(CLIENT_IDENTITY_PASSWORD_ENV),
            "{m}"
        );
        assert!(
            !m.contains("wrong-pass") && !m.contains(test_pki::PASSWORD),
            "{m}"
        );
        // A certificate without its key: an error, not a panic (native-tls on macOS panics).
        let m = err(
            "mqtts://h",
            flags(false, None, Some(&pki.cert_only_p12)),
            test_pki::PASSWORD,
        );
        assert!(m.contains("--mqtt-client-identity"), "{m}");
        // Not a file at all.
        let m = err("mqtts://h", flags(false, None, Some(&missing)), "");
        assert!(m.contains("missing.pem"), "{m}");
    }

    #[cfg(feature = "embedded-broker")]
    #[test]
    fn broker_identity_is_validated_up_front() {
        let pki = test_pki::make();
        assert!(broker_identity_with(&pki.server_p12, test_pki::PASSWORD).is_ok());
        for (path, pw) in [
            (&pki.server_p12, "wrong-pass"),
            (&pki.cert_only_p12, test_pki::PASSWORD),
            (&pki.ca_pem, test_pki::PASSWORD),
        ] {
            let m = format!("{:#}", broker_identity_with(path, pw).err().expect("must fail"));
            assert!(
                m.contains("--broker-tls-identity") && m.contains(BROKER_PASSWORD_ENV),
                "{m}"
            );
            assert!(!m.contains("wrong-pass"), "{m}");
        }
    }
}
