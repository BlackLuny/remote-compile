//! Client-side transport construction for the two gRPC clients (§16).
//!
//! `rc-server` speaks plain h2c and leaves TLS to a reverse proxy, so the
//! endpoint an agent or worker is configured with decides the scheme: a
//! loopback or in-datacentre hop stays `http://`, anything crossing the public
//! internet is `https://` and terminates at the proxy. Both clients need the
//! same rule, and getting it wrong means bearer tokens on the wire, so it
//! lives here rather than being duplicated twice.

use anyhow::{Context, Result};
use std::time::Duration;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

/// A dial that cannot complete must fail rather than park. Nothing above a
/// `connect()` has a clock of its own: the worker's retry loop, its systemd
/// unit and an agent's request all simply await it. A TCP handshake that lands
/// in a dead NAT mapping, or a proxy/SOCKS relay that accepts the socket and
/// then stops answering, otherwise leaves the caller waiting forever while
/// every supervisor keeps reporting "running" (risk #11).
///
/// 2026-11-01: the iMac worker sat in exactly that state for six days — socket
/// ESTABLISHED to the control plane, TLS/h2 handshake never completed, no
/// heartbeat, no log line, `rc-worker.service` still `active (running)` (§8.1).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle keepalive. A worker's channel is idle between tasks, which is when a
/// dropped flow goes unnoticed — there is no next request to time out. h2 PINGs
/// also keep the mapping warm, which is what relayed worker links need.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// An unanswered keepalive PING is a dead connection. Deliberately shorter than
/// the control plane's heartbeat timeout, so a session is torn down and rebuilt
/// before the scheduler writes the worker off.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Build an `Endpoint` for `server`, enabling TLS with the system trust store
/// when the URL asks for it, and bounding the time an unreachable or
/// half-open route may consume.
pub fn endpoint(server: &str) -> Result<Endpoint> {
    let ep = Channel::from_shared(server.to_string())
        .with_context(|| format!("invalid server url {server}"))?
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(Some(KEEPALIVE_INTERVAL))
        .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
        .keep_alive_timeout(KEEPALIVE_TIMEOUT)
        // Without this the keepalive stops on an idle channel — precisely the
        // state a worker waiting for work is in.
        .keep_alive_while_idle(true);
    if server.starts_with("https://") {
        // Native roots rather than a bundled webpki set: the proxy in front of
        // the control plane usually carries a Let's Encrypt certificate, and
        // operators expect their own CA to work by dropping it into the OS
        // store.
        let tls = ClientTlsConfig::new().with_native_roots();
        return ep
            .tls_config(tls)
            .with_context(|| format!("configure TLS for {server}"));
    }
    Ok(ep)
}
