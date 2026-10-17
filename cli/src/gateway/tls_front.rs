//! The embedded broker's TLS front end: it owns the public TLS port and terminates TLS itself, so
//! a peer that stalls its handshake holds up only its own connection.
//!
//! rumqttd runs each TLS handshake inline in its accept loop: one TCP client that connects and
//! never finishes its handshake keeps every other client off the port, so any host that can reach
//! it can deny service with one idle socket. Here the accept loop never waits on a handshake.
//! Each connection gets its own task, which does the handshake (native-tls, with the identity
//! loaded once at startup) within [`Limits::handshake_timeout`] and then copies bytes both ways to
//! a plain rumqttd listener on `127.0.0.1` that enforces the same login (see
//! `mqtt::embedded_broker`). Every connection takes a slot: at most [`Limits::handshakes`]
//! handshakes in flight and [`Limits::connections`] connections open. A connection past either
//! limit is closed at once, never queued behind the ones that hold the slots.
//!
//! Proxied clients reach rumqttd from `127.0.0.1`, which changes no decision: rumqttd applies no
//! per-address rule (its limits are per listener and per router, its login table is the same),
//! and the access policy (`mqtt::broker_access`) looks at the bind address, never at a peer's.
//! The limits that count against a peer sit here, in front of the hop. Nothing is logged per
//! connection, by rumqttd (there is no `tracing` subscriber) or here, so no log loses the real
//! address either.
//!
//! This is the crate's one piece of async code (the rest is plain threads): a current-thread
//! tokio runtime on a thread of its own, the tokio rumqttd and rumqttc already bring along. A
//! deadline on a handshake and a two-way copy of a TLS stream are what async does well; with
//! threads, native-tls's stream (one object for both directions) could only be polled.

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tokio_native_tls::{TlsAcceptor, TlsStream};

/// From TCP accept to a finished TLS handshake (and the connect to the broker behind it). An
/// honest handshake is two or three round trips and one signature, well under a second even on a
/// lossy Wi-Fi link that retransmits; 10 s keeps a wide margin over that, while a peer that never
/// finishes holds its slot for no longer.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// TLS handshakes in flight at once. An honest one holds its slot for milliseconds, so only a
/// flood fills them: to keep new clients out, a peer has to hold this many sockets open and renew
/// them every [`HANDSHAKE_TIMEOUT`], where one idle socket used to be enough. Below
/// [`MAX_CONNECTIONS`], so stalled handshakes alone never use up the connection slots.
pub(crate) const MAX_HANDSHAKES: usize = 32;

/// Connections open at once, handshaking or proxied. Each proxied one costs three descriptors in
/// this process (the client's socket and both ends of the hop to rumqttd): 64 stays within the
/// 256 a macOS shell grants by default, so a flood runs into this limit instead of descriptor
/// exhaustion (which would fail the serial port and the bridge too), and it is far more than the
/// clients a 16-node network needs (rumqttd's own limit, 128 sessions, covers every listener).
pub(crate) const MAX_CONNECTIONS: usize = 64;

/// How long a closing connection waits to send its TLS `close_notify` to a peer that may not be
/// reading.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// The pause after an accept error that is not about one connection (out of descriptors): the
/// pending connections wait in the backlog instead of spinning the loop.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The front end's limits: [`Limits::DEFAULT`] in the product; the tests shorten them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// See [`HANDSHAKE_TIMEOUT`].
    pub handshake_timeout: Duration,
    /// See [`MAX_HANDSHAKES`].
    pub handshakes: usize,
    /// See [`MAX_CONNECTIONS`].
    pub connections: usize,
}

impl Limits {
    pub(crate) const DEFAULT: Self = Self {
        handshake_timeout: HANDSHAKE_TIMEOUT,
        handshakes: MAX_HANDSHAKES,
        connections: MAX_CONNECTIONS,
    };
}

/// The running front end. Dropping it stops it: the accept loop and every connection task end
/// with its runtime, their sockets close, and the port is free once `drop` returns.
pub(crate) struct TlsFront {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for TlsFront {
    fn drop(&mut self) {
        // The accept loop races a receiver that completes once its sender is gone.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Serve TLS on `listener` (bound by the caller, which reports a bind error in its own words)
/// with `acceptor`, handing every connection to the plain MQTT listener at `upstream`.
pub(crate) fn start(
    listener: std::net::TcpListener,
    acceptor: native_tls::TlsAcceptor,
    upstream: SocketAddr,
    limits: Limits,
) -> Result<TlsFront> {
    let what = "starting the MQTT TLS front end";
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context(what)?;
    listener.set_nonblocking(true).context(what)?;
    let listener = {
        let _runtime = runtime.enter();
        TcpListener::from_std(listener).context(what)?
    };
    let acceptor = TlsAcceptor::from(acceptor);
    let (stop, stopped) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("mqtt-tls".into())
        .spawn(move || {
            runtime.block_on(async move {
                tokio::select! {
                    () = accept_loop(listener, acceptor, upstream, limits) => {}
                    _ = stopped => {}
                }
            });
            // Dropping the runtime here drops every connection task, and with it its sockets.
        })
        .context(what)?;
    Ok(TlsFront {
        stop: Some(stop),
        thread: Some(thread),
    })
}

/// Accept forever; never wait on anything but the next connection.
async fn accept_loop(listener: TcpListener, acceptor: TlsAcceptor, upstream: SocketAddr, limits: Limits) {
    let handshakes = Arc::new(Semaphore::new(limits.handshakes));
    let connections = Arc::new(Semaphore::new(limits.connections));
    loop {
        let tcp = match listener.accept().await {
            Ok((tcp, _)) => tcp,
            // One connection's failure (reset before it was accepted, say): just take the next.
            Err(e) if is_connection_error(&e) => continue,
            Err(_) => {
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        // Past a limit: dropped right here, which closes it.
        let (Ok(open), Ok(handshake)) = (
            Arc::clone(&connections).try_acquire_owned(),
            Arc::clone(&handshakes).try_acquire_owned(),
        ) else {
            continue;
        };
        tokio::spawn(connection(
            tcp,
            acceptor.clone(),
            upstream,
            limits.handshake_timeout,
            open,
            handshake,
        ));
    }
}

/// An accept error that concerns one connection only (the listener is fine).
fn is_connection_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset, Interrupted};
    matches!(
        e.kind(),
        ConnectionAborted | ConnectionRefused | ConnectionReset | Interrupted
    )
}

/// One client: the handshake and the hop to the broker within `timeout`, then the copy. `_open`
/// is its connection slot, held to the end; `handshake` its handshake slot, given back once the
/// handshake is over.
async fn connection(
    tcp: TcpStream,
    acceptor: TlsAcceptor,
    upstream: SocketAddr,
    timeout: Duration,
    _open: OwnedSemaphorePermit,
    handshake: OwnedSemaphorePermit,
) {
    // MQTT is small packets both ways: don't let Nagle hold them back at the extra hop.
    let _ = tcp.set_nodelay(true);
    let setup = async {
        let tls = acceptor.accept(tcp).await.ok()?;
        let broker = TcpStream::connect(upstream).await.ok()?;
        let _ = broker.set_nodelay(true);
        Some((tls, broker))
    };
    // A stalled, failed or plain-text handshake ends here, and its socket closes.
    let Ok(Some((tls, broker))) = tokio::time::timeout(timeout, setup).await else {
        return;
    };
    drop(handshake);
    proxy(tls, broker).await;
}

/// Copy both ways until either side is done; then the connection is over for both. An MQTT
/// session has no use for a half-open link, and a client that never closes its end must not keep
/// its slot after the broker ended the session (a wrong login, a missed keep-alive).
async fn proxy(tls: TlsStream<TcpStream>, broker: TcpStream) {
    let (mut from_client, mut to_client) = tokio::io::split(tls);
    let (mut from_broker, mut to_broker) = broker.into_split();
    tokio::select! {
        _ = tokio::io::copy(&mut from_client, &mut to_broker) => {}
        _ = tokio::io::copy(&mut from_broker, &mut to_client) => {
            // The broker closed: close the TLS layer too, without waiting on a peer that
            // doesn't read.
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, to_client.shutdown()).await;
        }
    }
}

/// A peer's view of its connection to the front end, for the tests here and in `mqtt`.
#[cfg(test)]
pub(crate) mod test_peer {
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    /// Read `s` until the front end closes it: the bytes it sent before closing (a TLS alert,
    /// say), or `None` if it stays open through `wait` of silence.
    pub(crate) fn closed_within(s: &mut TcpStream, wait: Duration) -> Option<Vec<u8>> {
        use std::io::ErrorKind::{ConnectionAborted, ConnectionReset};

        s.set_read_timeout(Some(wait)).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match s.read(&mut buf) {
                Ok(0) => return Some(got),
                Ok(n) => got.extend_from_slice(&buf[..n]),
                // Closed with our bytes unread: a reset instead of a FIN.
                Err(e) if matches!(e.kind(), ConnectionReset | ConnectionAborted) => return Some(got),
                // `WouldBlock` (Unix) or `TimedOut` (Windows): still open.
                Err(_) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Instant;

    use super::test_peer::closed_within;
    use super::*;
    use crate::gateway::tls::{self, test_pki};

    /// A front end on a free loopback port with the test PKI's server identity, in front of a
    /// port nothing listens on: these tests never get past a handshake.
    fn front(limits: Limits) -> (TlsFront, SocketAddr) {
        let pki = test_pki::make();
        let id = tls::broker_identity_with(&pki.server_p12, test_pki::PASSWORD).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let nowhere = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        (start(listener, id.acceptor, nowhere, limits).unwrap(), addr)
    }

    /// A peer that connects and never finishes its handshake (it sends nothing, or the start of
    /// a record and nothing after it) is closed at the deadline, and not before.
    #[test]
    fn a_stalled_handshake_is_closed_at_the_deadline() {
        let timeout = Duration::from_millis(300);
        let (_front, addr) = front(Limits {
            handshake_timeout: timeout,
            ..Limits::DEFAULT
        });
        for opening in [&[][..], &[0x16, 0x03, 0x01][..]] {
            let start = Instant::now();
            let mut staller = std::net::TcpStream::connect(addr).unwrap();
            staller.write_all(opening).unwrap();
            assert!(
                closed_within(&mut staller, Duration::from_secs(10)).is_some(),
                "{opening:02x?}: closed at the deadline"
            );
            let after = start.elapsed();
            assert!(
                after >= timeout,
                "{opening:02x?}: closed after {after:?}, before the deadline"
            );
        }
    }

    /// A connection past either limit (handshakes in flight, connections open) is closed at once,
    /// not queued behind the ones that hold the slots; those stay open (their deadline is far
    /// away).
    #[test]
    fn a_connection_past_a_limit_is_closed_at_once() {
        let hour = Duration::from_secs(3600);
        for limits in [
            Limits {
                handshake_timeout: hour,
                handshakes: 2,
                connections: 8,
            },
            Limits {
                handshake_timeout: hour,
                handshakes: 8,
                connections: 2,
            },
        ] {
            let (_front, addr) = front(limits);
            // Accepted in order: these two take the slots before the third is looked at.
            let mut held: Vec<_> = (0..2)
                .map(|_| std::net::TcpStream::connect(addr).unwrap())
                .collect();
            let mut extra = std::net::TcpStream::connect(addr).unwrap();
            assert!(
                closed_within(&mut extra, Duration::from_secs(10)).is_some(),
                "{limits:?}: the third connection is closed"
            );
            for s in &mut held {
                assert!(
                    closed_within(s, Duration::from_millis(100)).is_none(),
                    "{limits:?}: the slot holders stay open"
                );
            }
        }
    }
}
