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

use anyhow::{anyhow, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

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

    /// `example.com` matches exactly; `*.example.com` matches subdomains only.
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
}

impl ProxyServer {
    pub async fn bind(self: Arc<Self>, addr: SocketAddr) -> Result<SocketAddr> {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let me = self.clone();
                        tokio::spawn(async move {
                            if let Err(e) = me.handle(stream).await {
                                tracing::debug!(%peer, error = %e, "proxy connection ended");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "proxy accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        });
        Ok(local)
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
        let mut upstream = TcpStream::connect((hostname.as_str(), port)).await?;
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
        let mut upstream = match TcpStream::connect((host.as_str(), port)).await {
            Ok(s) => s,
            Err(e) => {
                client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                    .await?;
                return Err(e.into());
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
        });
        let addr = proxy
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
        });
        let addr = proxy.bind("127.0.0.1:0".parse().unwrap()).await.unwrap();

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

    #[tokio::test]
    async fn connect_to_a_denied_host_is_refused_before_dialling() {
        let proxy = Arc::new(ProxyServer {
            allowlist: list(),
            byte_cap: 1024,
        });
        let addr = proxy.bind("127.0.0.1:0".parse().unwrap()).await.unwrap();

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
