//! Client-side transport construction for the two gRPC clients (§16).
//!
//! `rc-server` speaks plain h2c and leaves TLS to a reverse proxy, so the
//! endpoint an agent or worker is configured with decides the scheme: a
//! loopback or in-datacentre hop stays `http://`, anything crossing the public
//! internet is `https://` and terminates at the proxy. Both clients need the
//! same rule, and getting it wrong means bearer tokens on the wire, so it
//! lives here rather than being duplicated twice.

use anyhow::{Context, Result};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

/// Build an `Endpoint` for `server`, enabling TLS with the system trust store
/// when the URL asks for it.
pub fn endpoint(server: &str) -> Result<Endpoint> {
    let ep = Channel::from_shared(server.to_string())
        .with_context(|| format!("invalid server url {server}"))?;
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
