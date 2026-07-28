//! Allowlisted egress proxy (§7.1).
//!
//! Build containers sit on an `internal` Docker network whose only reachable
//! address is the host gateway, where this proxy listens. Dependencies can be
//! fetched; nothing else is routable.
//!
//! ## What this can and cannot enforce
//!
//! For cleartext HTTP the proxy sees the full request and enforces both host
//! and method: GET/HEAD only, plus the POST that git's smart-HTTP
//! `git-upload-pack` needs. Blanket POST access to github.com would hand a
//! malicious `build.rs` a push channel.
//!
//! For HTTPS the client issues CONNECT and the proxy only sees the host name —
//! method filtering inside the tunnel would require TLS interception, which v0
//! does not do. Host allowlisting and a per-tunnel byte cap still apply. As
//! §16 states, a GET channel can always encode a small amount of data
//! outbound; this narrows the pipe rather than closing it.
//!
//! ## Who may use a proxy
//!
//! Every build container sits on the same `rc-egress` bridge and can route to
//! the gateway — that is the network's whole design. So *binding* a proxy there
//! does not decide who reaches it: while one task's widened proxy is listening,
//! any container on the bridge can find the port and use it. The fleet-wide
//! proxy does not care (every task gets the same list), but a proxy carrying
//! one project's approved hosts must not serve another project's build. Those
//! proxies therefore require a per-task credential, handed to exactly one
//! container in its `http_proxy` URL. Network position is not identity.

use anyhow::{anyhow, Result};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

/// The ports the proxy will dial unless an operator says otherwise. Egress
/// entries name hosts, never ports, and a build fetches over HTTP(S) —
/// `CONNECT internal-host:22` is not a dependency fetch. Overridable because a
/// fleet allowlist may legitimately name an internal mirror on `:8081`, and
/// turning that into a 403 on upgrade would be a silent feature removal.
pub const DEFAULT_DIALABLE_PORTS: [u16; 2] = [80, 443];

#[derive(Debug, Clone)]
pub struct Allowlist {
    entries: Vec<String>,
}

impl Allowlist {
    pub fn new(entries: impl IntoIterator<Item = String>) -> Self {
        Allowlist {
            entries: entries
                .into_iter()
                .map(|e| e.trim().to_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
        }
    }

    /// `example.com` matches exactly; `*.example.com` matches subdomains and
    /// the bare domain, which is what `rc_core::egress` documents as the
    /// meaning of the wildcard an administrator approves.
    pub fn allows(&self, host: &str) -> bool {
        let host = host.split(':').next().unwrap_or(host).to_lowercase();
        self.entries.iter().any(|e| match e.strip_prefix("*.") {
            Some(suffix) => host.ends_with(&format!(".{suffix}")) || host == suffix,
            None => host == *e,
        })
    }
}

/// Methods that cannot push data into a remote repository.
pub fn method_allowed(method: &str, path: &str) -> bool {
    match method {
        "GET" | "HEAD" => true,
        // git smart HTTP negotiates a fetch with a POST; it is the one write
        // verb a read-only mirror genuinely needs.
        "POST" => path.ends_with("/git-upload-pack"),
        _ => false,
    }
}

pub struct ProxyServer {
    pub allowlist: Allowlist,
    pub byte_cap: u64,
    /// `Some` for a task-scoped proxy: the credential the one container this
    /// proxy belongs to was given. `None` for the fleet proxy, which carries
    /// nothing project-specific.
    pub credential: Option<String>,
    /// Ports the proxy may dial.
    pub ports: Vec<u16>,
    /// Let the proxy dial loopback, private and link-local addresses. Off by
    /// default: a name is not a promise about where it points, and the whole
    /// value of the sandbox is that a build cannot reach the worker's own
    /// network. An operator running a genuinely internal registry on an RFC1918
    /// address turns this on deliberately.
    pub allow_private: bool,
}

/// Addresses a build has no business reaching through us, whatever name
/// pointed at them. `127.0.0.1.nip.io` is an ordinary host name that resolves
/// to loopback; no validator upstream of here can catch that, so the check
/// belongs at the moment we are about to dial.
fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            // 0.0.0.0/8 in full, not just the unspecified address: Linux's
            // local table treats the whole block as this-host, so `0.1.2.3`
            // dials loopback.
            v4.octets()[0] == 0
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                // 100.64/10, carrier-grade NAT: not private per `is_private`,
                // but not somewhere a build should be dialling either.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_internal(IpAddr::V4(mapped));
            }
            let seg = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fc00::/7 unique-local and fe80::/10 link-local; the stable
                // library predicates for these are still unstable.
                || (seg & 0xfe00) == 0xfc00
                || (seg & 0xffc0) == 0xfe80
        }
    }
}

/// Resolve and connect, refusing any name that points somewhere internal.
///
/// A name that resolves to *both* a public and an internal address is refused
/// outright rather than filtered down to the public one — that shape is a
/// rebinding attempt, not a configuration.
async fn dial(host: &str, port: u16, ports: &[u16], allow_private: bool) -> Result<TcpStream> {
    if !ports.contains(&port) {
        return Err(anyhow!(
            "the egress proxy dials {ports:?} only, not {port}"
        ));
    }
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| anyhow!("resolve {host}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(anyhow!("{host} resolved to nothing"));
    }
    if !allow_private {
        if let Some(bad) = addrs.iter().find(|a| is_internal(a.ip())) {
            return Err(anyhow!(
                "{host} resolves to {}, which is inside the worker's own network",
                bad.ip()
            ));
        }
    }
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(anyhow!("connect {host}:{port}: {}", last.expect("addrs is non-empty")))
}

/// RFC4648 base64, just enough to build the `Basic` credential we expect.
/// Pulling a dependency in for twelve lines would be the worse trade.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The `Proxy-Authorization` value a container holding `credential` will send.
pub fn expected_authorization(credential: &str) -> String {
    format!("Basic {}", base64(format!("rc:{credential}").as_bytes()))
}

/// Compare without an early return, so a wrong credential cannot be recovered
/// one character at a time.
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// Start a proxy on the egress bridge gateway and return the URL a container
/// should use, plus the handle that keeps it alive.
///
/// Binding on the gateway rather than on all interfaces is what makes this
/// reachable only from the `internal` network the build containers sit on.
/// `credential` is `Some` for a task-scoped proxy; it is embedded in the
/// returned URL, which becomes exactly one container's `http_proxy`.
pub async fn listen(
    gateway: String,
    allowlist: Vec<String>,
    byte_cap: u64,
    credential: Option<String>,
    ports: Vec<u16>,
    allow_private: bool,
) -> Result<(String, ProxyHandle)> {
    let server = std::sync::Arc::new(ProxyServer {
        allowlist: Allowlist::new(allowlist),
        byte_cap,
        credential: credential.clone(),
        ports,
        allow_private,
    });
    let addr: SocketAddr = format!("{gateway}:0")
        .parse()
        .map_err(|e| anyhow!("parse the egress gateway address {gateway}: {e}"))?;
    let (bound, handle) = server.bind(addr).await?;
    let url = match &credential {
        Some(c) => format!("http://rc:{c}@{gateway}:{}", bound.port()),
        None => format!("http://{gateway}:{}", bound.port()),
    };
    Ok((url, handle))
}

/// Keeps a listener alive. Dropping it stops accepting *and* tears down every
/// connection it accepted, which is how a task-scoped proxy stops existing the
/// moment its build is over: an allowlist approved for one project must not be
/// reachable by the next task to land on this worker, and a CONNECT tunnel
/// opened during the build would otherwise outlive the handle that authorised
/// it.
#[derive(Debug)]
pub struct ProxyHandle(tokio::task::JoinHandle<()>);

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ProxyServer {
    pub async fn bind(self: Arc<Self>, addr: SocketAddr) -> Result<(SocketAddr, ProxyHandle)> {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let task = tokio::spawn(async move {
            // Owning the connection tasks is what makes aborting the accept
            // loop enough: dropping the set cancels them with it.
            let mut conns = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok((stream, peer)) => {
                            let me = self.clone();
                            conns.spawn(async move {
                                if let Err(e) = me.handle(stream).await {
                                    tracing::debug!(%peer, error = %e, "proxy connection ended");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "proxy accept failed");
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                    },
                    // Reap finished connections so the set does not grow for
                    // the length of the build.
                    _ = conns.join_next(), if !conns.is_empty() => {}
                }
            }
        });
        Ok((local, ProxyHandle(task)))
    }

    /// A task-scoped proxy serves only the container it was created for.
    fn authorized(&self, headers: &[String]) -> bool {
        let Some(credential) = &self.credential else {
            return true;
        };
        let expected = expected_authorization(credential);
        headers.iter().any(|h| {
            h.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("proxy-authorization"))
                .is_some_and(|(_, value)| secret_eq(value.trim(), &expected))
        })
    }

    async fn handle(&self, client: TcpStream) -> Result<()> {
        let mut reader = BufReader::new(client);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).await?;
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 3 {
            return deny(reader.into_inner(), 400, "malformed request").await;
        }
        let (method, target) = (parts[0].to_uppercase(), parts[1].to_string());

        // Read the rest of the head so we can forward it verbatim.
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            headers.push(line);
        }

        if !self.authorized(&headers) {
            tracing::warn!(%method, "egress denied: no valid proxy credential");
            return challenge(reader.into_inner()).await;
        }

        if method == "CONNECT" {
            return self.handle_connect(reader.into_inner(), &target).await;
        }

        let (host, path) = split_absolute_uri(&target)
            .ok_or_else(|| anyhow!("proxy requires absolute-form URIs, got {target}"))?;
        if !self.allowlist.allows(&host) {
            tracing::warn!(%host, "egress denied: host not in allowlist");
            return deny(reader.into_inner(), 403, "host is not in the egress allowlist").await;
        }
        if !method_allowed(&method, &path) {
            tracing::warn!(%host, %method, "egress denied: method not permitted");
            return deny(
                reader.into_inner(),
                403,
                "only GET/HEAD (and git-upload-pack POST) may leave the sandbox",
            )
            .await;
        }

        let port = host.split(':').nth(1).and_then(|p| p.parse().ok()).unwrap_or(80);
        let hostname = host.split(':').next().unwrap_or(&host).to_string();
        let mut upstream = match dial(&hostname, port, &self.ports, self.allow_private).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%hostname, error = %e, "egress denied at dial");
                return deny(reader.into_inner(), 403, "the host is not dialable from here").await;
            }
        };
        let mut head = format!("{method} {path} HTTP/1.1\r\n");
        for h in &headers {
            let lower = h.to_lowercase();
            if lower.starts_with("proxy-connection:") || lower.starts_with("proxy-authorization:") {
                continue;
            }
            head.push_str(h);
        }
        head.push_str("\r\n");
        upstream.write_all(head.as_bytes()).await?;

        let mut client = reader.into_inner();
        let (mut cr, mut cw) = client.split();
        let (mut ur, mut uw) = upstream.split();
        let cap = self.byte_cap;
        let up = capped_copy(&mut cr, &mut uw, cap);
        let down = capped_copy(&mut ur, &mut cw, cap);
        let _ = tokio::join!(up, down);
        Ok(())
    }

    async fn handle_connect(&self, mut client: TcpStream, target: &str) -> Result<()> {
        if !self.allowlist.allows(target) {
            tracing::warn!(%target, "egress denied: CONNECT host not in allowlist");
            client
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .await?;
            return Ok(());
        }
        let host = target.split(':').next().unwrap_or(target).to_string();
        let port: u16 = target.split(':').nth(1).and_then(|p| p.parse().ok()).unwrap_or(443);
        let mut upstream = match dial(&host, port, &self.ports, self.allow_private).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%host, %port, error = %e, "egress denied at dial");
                client
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                    .await?;
                return Ok(());
            }
        };
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;

        let (mut cr, mut cw) = client.split();
        let (mut ur, mut uw) = upstream.split();
        let cap = self.byte_cap;
        let up = capped_copy(&mut cr, &mut uw, cap);
        let down = capped_copy(&mut ur, &mut cw, cap);
        let _ = tokio::join!(up, down);
        Ok(())
    }
}

/// 407 rather than 403, with the challenge, so a client that waits to be asked
/// for proxy credentials rather than sending them preemptively still works.
async fn challenge(mut client: TcpStream) -> Result<()> {
    let body = "remote-compile egress proxy: this proxy belongs to another task\n";
    let head = format!(
        "HTTP/1.1 407 Proxy Authentication Required\r\n\
         Proxy-Authenticate: Basic realm=\"remote-compile\"\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes()).await?;
    client.write_all(body.as_bytes()).await?;
    Ok(())
}

async fn deny(mut client: TcpStream, code: u16, reason: &str) -> Result<()> {
    let body = format!("remote-compile egress proxy: {reason}\n");
    let head = format!(
        "HTTP/1.1 {code} Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes()).await?;
    client.write_all(body.as_bytes()).await?;
    Ok(())
}

/// Copy until EOF or the cap is hit. The cap is the only bound on how much a
/// build can push out through an allowed host.
async fn capped_copy<R, W>(reader: &mut R, writer: &mut W, cap: u64) -> Result<u64>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 32 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > cap {
            tracing::warn!(cap, "egress byte cap exceeded; cutting the connection");
            break;
        }
        writer.write_all(&buf[..n]).await?;
        writer.flush().await?;
    }
    let _ = writer.shutdown().await;
    Ok(total)
}

fn split_absolute_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("http://").or_else(|| uri.strip_prefix("https://"))?;
    match rest.find('/') {
        Some(i) => Some((rest[..i].to_string(), rest[i..].to_string())),
        None => Some((rest.to_string(), "/".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Allowlist {
        Allowlist::new(vec![
            "crates.io".to_string(),
            "*.githubusercontent.com".to_string(),
        ])
    }

    #[test]
    fn exact_hosts_match() {
        assert!(list().allows("crates.io"));
        assert!(list().allows("crates.io:443"));
        assert!(!list().allows("evil.com"));
    }

    #[test]
    fn a_lookalike_suffix_is_not_a_match() {
        // "notcrates.io" must not slip through an endswith check.
        assert!(!list().allows("notcrates.io"));
        assert!(!list().allows("crates.io.evil.com"));
    }

    #[test]
    fn wildcards_cover_subdomains_and_the_apex() {
        assert!(list().allows("objects.githubusercontent.com"));
        assert!(list().allows("githubusercontent.com"));
        assert!(!list().allows("githubusercontent.com.evil.net"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(list().allows("CRATES.IO"));
    }

    #[test]
    fn an_empty_allowlist_denies_everything() {
        let empty = Allowlist::new(Vec::<String>::new());
        assert!(!empty.allows("crates.io"));
    }

    #[test]
    fn only_read_methods_leave_the_sandbox() {
        assert!(method_allowed("GET", "/api/v1/crates/serde/1.0.0/download"));
        assert!(method_allowed("HEAD", "/"));
        // A blanket POST to github would be a source-exfiltration channel.
        assert!(!method_allowed("POST", "/org/repo/git-receive-pack"));
        assert!(!method_allowed("PUT", "/anything"));
        assert!(!method_allowed("DELETE", "/anything"));
    }

    #[test]
    fn git_fetch_negotiation_is_the_one_permitted_post() {
        assert!(method_allowed("POST", "/org/repo.git/git-upload-pack"));
        assert!(!method_allowed("POST", "/org/repo.git/git-upload-pack/extra"));
    }

    #[test]
    fn absolute_uris_split_into_host_and_path() {
        assert_eq!(
            split_absolute_uri("http://crates.io/api/v1/x"),
            Some(("crates.io".into(), "/api/v1/x".into()))
        );
        assert_eq!(
            split_absolute_uri("http://crates.io"),
            Some(("crates.io".into(), "/".into()))
        );
        assert!(split_absolute_uri("/relative/path").is_none());
    }

    #[tokio::test]
    async fn a_denied_host_gets_403_and_no_upstream_connection() {
        let proxy = Arc::new(ProxyServer {
            allowlist: list(),
            byte_cap: 1024,
            credential: None,
            ports: DEFAULT_DIALABLE_PORTS.to_vec(),
            allow_private: false,
        });
        // `_guard` keeps the listener alive for the length of the test: the
        // handle stops accepting when it is dropped.
        let (addr, _guard) = proxy
            .bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET http://evil.com/steal HTTP/1.1\r\nHost: evil.com\r\n\r\n")
            .await
            .unwrap();
        let mut buf = String::new();
        client.read_to_string(&mut buf).await.unwrap();
        assert!(buf.starts_with("HTTP/1.1 403"), "{buf}");
        assert!(buf.contains("allowlist"));
    }

    #[tokio::test]
    async fn a_write_method_to_an_allowed_host_is_still_refused() {
        let proxy = Arc::new(ProxyServer {
            allowlist: list(),
            byte_cap: 1024,
            credential: None,
            ports: DEFAULT_DIALABLE_PORTS.to_vec(),
            allow_private: false,
        });
        let (addr, _guard) = proxy.bind("127.0.0.1:0".parse().unwrap()).await.unwrap();

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"PUT http://crates.io/upload HTTP/1.1\r\nHost: crates.io\r\n\r\n")
            .await
            .unwrap();
        let mut buf = String::new();
        client.read_to_string(&mut buf).await.unwrap();
        assert!(buf.starts_with("HTTP/1.1 403"), "{buf}");
        assert!(buf.contains("GET/HEAD"));
    }

    #[test]
    fn addresses_inside_the_workers_own_network_are_recognised() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.17.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            // The whole of 0/8 is this-host on Linux, not just 0.0.0.0 — a name
            // flipped to `0.1.2.3` would otherwise dial loopback.
            "0.1.2.3",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_internal(ip.parse().unwrap()), "{ip} must be refused");
        }
        for ip in ["1.1.1.1", "140.82.114.4", "2606:4700:4700::1111"] {
            assert!(!is_internal(ip.parse().unwrap()), "{ip} must be dialable");
        }
    }

    #[tokio::test]
    async fn a_name_that_resolves_into_the_worker_is_refused_however_it_is_spelled() {
        // The allowlist cannot defend against this: `localhost` here stands in
        // for any ordinary-looking name whose DNS points at loopback, which is
        // a thing anyone can publish. The check has to happen at dial time.
        let err = dial("localhost", 80, &DEFAULT_DIALABLE_PORTS, false).await.unwrap_err().to_string();
        assert!(err.contains("worker's own network"), "{err}");
        // …and an operator who means it can still turn it on.
        assert!(!dial("localhost", 80, &DEFAULT_DIALABLE_PORTS, true)
            .await
            .err()
            .map(|e| e.to_string().contains("worker's own network"))
            .unwrap_or(false));
    }

    #[tokio::test]
    async fn the_proxy_dials_web_ports_only() {
        let err = dial("example.com", 22, &DEFAULT_DIALABLE_PORTS, false).await.unwrap_err().to_string();
        assert!(err.contains("not 22"), "{err}");
    }

    #[tokio::test]
    async fn a_task_scoped_proxy_serves_nobody_without_its_credential() {
        // The co-resident container's view: it found the port, it knows the
        // allowlist is wider, and none of that is enough.
        let proxy = Arc::new(ProxyServer {
            allowlist: list(),
            byte_cap: 1024,
            credential: Some("SECRET".into()),
            ports: DEFAULT_DIALABLE_PORTS.to_vec(),
            allow_private: false,
        });
        let (addr, _guard) = proxy.bind("127.0.0.1:0".parse().unwrap()).await.unwrap();

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET http://crates.io/api HTTP/1.1\r\nHost: crates.io\r\n\r\n")
            .await
            .unwrap();
        let mut buf = String::new();
        client.read_to_string(&mut buf).await.unwrap();
        assert!(buf.starts_with("HTTP/1.1 407"), "{buf}");

        // A guessed credential is no better than none.
        let mut client = TcpStream::connect(addr).await.unwrap();
        let wrong = expected_authorization("NOT-THE-SECRET");
        client
            .write_all(
                format!("CONNECT crates.io:443 HTTP/1.1\r\nProxy-Authorization: {wrong}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut buf = String::new();
        client.read_to_string(&mut buf).await.unwrap();
        assert!(buf.starts_with("HTTP/1.1 407"), "{buf}");
    }

    #[tokio::test]
    async fn the_container_holding_the_credential_gets_through_to_the_allowlist_check() {
        let proxy = Arc::new(ProxyServer {
            allowlist: list(),
            byte_cap: 1024,
            credential: Some("SECRET".into()),
            ports: DEFAULT_DIALABLE_PORTS.to_vec(),
            allow_private: false,
        });
        let (addr, _guard) = proxy.bind("127.0.0.1:0".parse().unwrap()).await.unwrap();

        let mut client = TcpStream::connect(addr).await.unwrap();
        let auth = expected_authorization("SECRET");
        client
            .write_all(
                format!("GET http://evil.com/steal HTTP/1.1\r\nProxy-Authorization: {auth}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut buf = String::new();
        client.read_to_string(&mut buf).await.unwrap();
        // Past authentication, and stopped by the allowlist rather than by 407:
        // the credential decides *who*, the allowlist still decides *where*.
        assert!(buf.starts_with("HTTP/1.1 403"), "{buf}");
        assert!(buf.contains("allowlist"), "{buf}");
    }

    #[test]
    fn a_wrong_credential_cannot_be_recovered_one_character_at_a_time() {
        let expected = expected_authorization("SECRET");
        assert!(secret_eq(&expected, &expected));
        assert!(!secret_eq(&expected, ""));
        assert!(!secret_eq(&expected, &expected[..expected.len() - 1]));
        assert!(!secret_eq(&expected, &format!("{expected}x")));
        // A near miss in the last position must be no more equal than any other.
        let mut near = expected.clone().into_bytes();
        *near.last_mut().unwrap() ^= 1;
        assert!(!secret_eq(&expected, &String::from_utf8(near).unwrap()));
    }

    #[tokio::test]
    async fn the_task_credential_is_not_forwarded_to_the_upstream_host() {
        // The one line keeping a task's credential out of every allowed host's
        // access log. Nothing else in the file would notice it going missing.
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            let (mut sock, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });

        let proxy = Arc::new(ProxyServer {
            allowlist: Allowlist::new(vec!["localhost".to_string()]),
            byte_cap: 4096,
            credential: Some("SECRET".into()),
            ports: vec![upstream_addr.port()],
            // The upstream is on loopback, which is the only way to observe
            // what actually leaves the proxy.
            allow_private: true,
        });
        let (addr, _guard) = proxy.bind("127.0.0.1:0".parse().unwrap()).await.unwrap();

        let mut client = TcpStream::connect(addr).await.unwrap();
        let auth = expected_authorization("SECRET");
        client
            .write_all(
                format!(
                    "GET http://localhost:{}/index HTTP/1.1\r\nHost: localhost\r\n\
                     Proxy-Authorization: {auth}\r\nUser-Agent: cargo\r\n\r\n",
                    upstream_addr.port()
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let forwarded = seen.await.unwrap();
        assert!(forwarded.contains("User-Agent: cargo"), "{forwarded}");
        assert!(
            !forwarded.to_lowercase().contains("proxy-authorization"),
            "the credential reached the upstream host: {forwarded}"
        );
    }

    #[test]
    fn the_credential_is_encoded_the_way_a_proxy_client_sends_it() {
        // `curl -x http://rc:SECRET@host:port` puts exactly this on the wire.
        assert_eq!(expected_authorization("SECRET"), "Basic cmM6U0VDUkVU");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }

    #[tokio::test]
    async fn connect_to_a_denied_host_is_refused_before_dialling() {
        let proxy = Arc::new(ProxyServer {
            allowlist: list(),
            byte_cap: 1024,
            credential: None,
            ports: DEFAULT_DIALABLE_PORTS.to_vec(),
            allow_private: false,
        });
        let (addr, _guard) = proxy.bind("127.0.0.1:0".parse().unwrap()).await.unwrap();

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"CONNECT evil.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = String::new();
        client.read_to_string(&mut buf).await.unwrap();
        assert!(buf.starts_with("HTTP/1.1 403"), "{buf}");
    }
}
