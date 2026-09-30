//! Agent-side proxy transport. The destination still uses tonic's TLS verifier.
use anyhow::{bail, Context, Result};
use http::Uri;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::rt::TokioIo;
use hyper_util::client::proxy::matcher::Matcher;
use std::time::Duration;
use tonic::transport::{Channel, Endpoint};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tower::{service_fn, ServiceExt};

/// Match standard HTTP(S)_PROXY / ALL_PROXY / NO_PROXY environment variables.
/// Proxy credentials stay in a sensitive CONNECT header, never the origin URI.
pub async fn connect(server: &str) -> Result<Channel> {
    let endpoint = rc_core::transport::endpoint(server)?;
    let no_proxy = first_env(&["NO_PROXY", "no_proxy"]);
    let matcher = normalize_no_proxy(Matcher::from_env(), &no_proxy);
    let specific = match endpoint.uri().scheme_str() {
        Some("https") => first_env(&["HTTPS_PROXY", "https_proxy"]),
        Some("http") => first_env(&["HTTP_PROXY", "http_proxy"]),
        _ => String::new(),
    };
    let selected = if specific.is_empty() { first_env(&["ALL_PROXY", "all_proxy"]) } else { specific };
    validate_proxy_route(endpoint.uri(), &matcher, &selected, &no_proxy)?;
    connect_with_matcher(endpoint, matcher).await
}

fn first_env(names: &[&str]) -> String {
    names.iter().find_map(|name| std::env::var(name).ok()).unwrap_or_default()
}

fn no_proxy_all(no_proxy: &str) -> bool {
    no_proxy.split(',').any(|entry| entry.trim() == "*")
}

fn normalize_no_proxy(matcher: Matcher, no_proxy: &str) -> Matcher {
    // hyper-util 0.1.20 applies '*' to domains but misses IP literals.
    if no_proxy_all(no_proxy) { Matcher::builder().build() } else { matcher }
}

fn validate_proxy_route(uri: &Uri, matcher: &Matcher, selected: &str, no_proxy: &str) -> Result<()> {
    // The library ignores malformed/unknown proxy URLs. That must not turn an
    // explicitly configured proxy into a silent direct connection. NO_PROXY
    // still intentionally selects direct transport, including its '*' form.
    let bypassed = no_proxy_all(no_proxy) || Matcher::builder().all("http://proxy.invalid").no(no_proxy).build().intercept(uri).is_none();
    if selected.is_empty() || bypassed { return Ok(()); }
    // Validate the selected variable itself before the matcher can discard it
    // and fall back to ALL_PROXY. Never include its value in diagnostics.
    let parsed = Matcher::builder().all(selected).build().intercept(uri);
    if !matches!(parsed.as_ref().map(|p| p.uri().scheme_str()), Some(Some("http" | "https"))) {
        bail!("configured proxy is invalid or unsupported; refusing a direct connection or fallback proxy");
    }
    if matcher.intercept(uri).is_none() {
        bail!("configured proxy is disabled in this environment; refusing a direct connection");
    }
    Ok(())
}

async fn connect_with_matcher(endpoint: Endpoint, matcher: Matcher) -> Result<Channel> {
    let Some(proxy) = matcher.intercept(endpoint.uri()) else {
        return endpoint.connect_timeout(Duration::from_secs(5)).connect().await
            .context("direct gRPC connection failed");
    };
    if !matches!(proxy.uri().scheme_str(), Some("http" | "https")) {
        bail!("configured proxy scheme is unsupported; use an HTTP or HTTPS CONNECT proxy");
    }
    // TLS to an HTTPS proxy is separately verified. TLS to the destination is
    // applied by Endpoint::connect_with_connector after CONNECT succeeds.
    let connector = HttpsConnectorBuilder::new()
        .with_native_roots().context("load native roots for HTTPS proxy")?
        .https_or_http().enable_http1().build();
    let proxy_uri = proxy.uri().clone();
    let auth = proxy.basic_auth().cloned();
    let target = connect_target(endpoint.uri())?;
    let connector = service_fn(move |_uri: Uri| {
        let connector = connector.clone();
        let proxy_uri = proxy_uri.clone();
        let auth = auth.clone();
        let target = target.clone();
        async move {
            let stream = connector.oneshot(proxy_uri).await?;
            let stream = establish_tunnel(TokioIo::new(stream), &target, auth.as_ref()).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(TokioIo::new(stream))
        }
    });
    // A cold CONNECT proxy can be slower than a direct TCP connection. Bound
    // the entire CONNECT + verified TLS handshake; never fall back to direct.
    endpoint.connect_timeout(Duration::from_secs(30))
        .connect_with_connector(connector).await
        .context("gRPC connection through configured HTTP(S) CONNECT proxy failed")
}

// Keep bytes following the CONNECT headers: an h2c server can send its SETTINGS
// in the same read. Hyper-util 0.1.20's Tunnel discards/blocks on such bytes.
// Read through a BufReader so fragmented headers and early payload both work.
async fn establish_tunnel<S>(mut stream: S, target: &Uri, auth: Option<&http::HeaderValue>)
    -> std::io::Result<BufReader<S>>
where S: AsyncRead + AsyncWrite + Unpin {
    let authority = target.authority().ok_or_else(|| std::io::Error::other("missing CONNECT authority"))?;
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n").into_bytes();
    if let Some(auth) = auth {
        request.extend_from_slice(b"Proxy-Authorization: ");
        request.extend_from_slice(auth.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    stream.write_all(&request).await?;
    stream.flush().await?;
    let mut stream = BufReader::new(stream);
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        if response.len() == 8192 {
            return Err(std::io::Error::other("proxy CONNECT response headers exceed 8192 bytes"));
        }
        response.push(stream.read_u8().await?);
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers);
    let status = parsed.parse(&response).map_err(|_| std::io::Error::other("invalid proxy CONNECT response"))?;
    if !status.is_complete() {
        return Err(std::io::Error::other("incomplete proxy CONNECT response"));
    }
    match parsed.code {
        Some(200..=299) => Ok(stream),
        Some(407) => Err(std::io::Error::other("proxy authentication required")),
        Some(code) => Err(std::io::Error::other(format!("proxy rejected CONNECT with HTTP {code}"))),
        None => Err(std::io::Error::other("missing proxy CONNECT response status")),
    }
}

fn connect_target(uri: &Uri) -> Result<Uri> {
    if uri.port_u16().is_some() { return Ok(uri.clone()); }
    let port = match uri.scheme_str() {
        Some("http") => 80,
        Some("https") => 443,
        _ => bail!("gRPC endpoint must use HTTP or HTTPS"),
    };
    let host = uri.host().context("gRPC endpoint has no host")?;
    let mut parts = uri.clone().into_parts();
    parts.authority = Some(format!("{host}:{port}").parse().context("invalid CONNECT authority")?);
    Uri::from_parts(parts).context("invalid CONNECT destination")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::body::Bytes;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use rc_core::pb::{agent_api_client::AgentApiClient, Empty};
    use std::{convert::Infallible, sync::Arc};
    use tokio::{io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt}, net::TcpListener};

    async fn grpc_unauthenticated<S>(stream: S)
    where S: AsyncRead + AsyncWrite + Send + Unpin + 'static {
        let service = hyper::service::service_fn(|_| async {
            Ok::<_, Infallible>(http::Response::builder().status(200)
                .header("content-type", "application/grpc")
                .header("grpc-status", "16")
                .header("grpc-message", "test authentication required")
                .body(http_body_util::Empty::<Bytes>::new()).unwrap())
        });
        let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(stream), service).await;
    }

    async fn connect_request<S: AsyncRead + Unpin>(stream: &mut S) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            assert!(bytes.len() < 8192);
            bytes.push(stream.read_u8().await.unwrap());
        }
        String::from_utf8(bytes).unwrap()
    }

    async fn assert_rpc_reaches_server(channel: Channel) {
        let error = AgentApiClient::new(channel).list_workers(Empty {}).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unauthenticated);
        assert_eq!(error.message(), "test authentication required");
    }

    #[test]
    fn default_connect_ports_are_correct() {
        for (input, authority) in [("http://example.com/", "example.com:80"),
            ("https://example.com/", "example.com:443"),
            ("https://example.com:8443/", "example.com:8443"),
            ("https://[::1]/", "[::1]:443")] {
            let uri = connect_target(&input.parse().unwrap()).unwrap();
            assert_eq!(uri.authority().unwrap().as_str(), authority);
        }
    }

    #[test]
    fn no_proxy_matching_and_proxy_precedence() {
        let matcher = Matcher::builder().all("http://all-proxy:8080")
            .https("http://https-proxy:8080")
            .no("localhost,.example.com,10.0.0.0/8").build();
        for url in ["http://localhost/", "https://example.com/", "https://a.example.com/", "http://10.2.3.4/"] {
            assert!(matcher.intercept(&url.parse().unwrap()).is_none());
        }
        assert_eq!(matcher.intercept(&"https://notexample.com/".parse().unwrap()).unwrap().uri().host(), Some("https-proxy"));
        assert_eq!(matcher.intercept(&"http://elsewhere/".parse().unwrap()).unwrap().uri().host(), Some("all-proxy"));
        assert!(Matcher::builder().all("http://proxy:8080").no("*").build()
            .intercept(&"https://anything/".parse().unwrap()).is_none());
    }

    #[test]
    fn proxy_credentials_are_sensitive_and_debug_redacted() {
        let matcher = Matcher::builder().https("http://proxyuser:proxypass@127.0.0.1:8080").build();
        let proxy = matcher.intercept(&"https://origin/".parse().unwrap()).unwrap();
        assert!(proxy.basic_auth().unwrap().is_sensitive());
        for debug in [format!("{matcher:?}"), format!("{proxy:?}"), format!("{:?}", proxy.basic_auth())] {
            assert!(!debug.contains("proxyuser"));
            assert!(!debug.contains("proxypass"));
            assert!(!debug.contains("cHJveHl1c2VyOnByb3h5cGFzcw=="));
        }
        assert!(!proxy.uri().to_string().contains('@'));
    }

    #[test]
    fn invalid_proxy_configuration_cannot_silently_select_direct() {
        let uri = "https://origin.invalid".parse().unwrap();
        for value in ["ftp://proxy.invalid", "http://", "not a valid proxy"] {
            let matcher = Matcher::builder().https(value).build();
            let error = validate_proxy_route(&uri, &matcher, value, "").unwrap_err();
            assert!(error.to_string().contains("refusing a direct connection"));
        }
    }

    #[test]
    fn explicit_no_proxy_remains_valid_even_with_invalid_proxy() {
        let uri = "https://origin.invalid".parse().unwrap();
        let matcher = Matcher::builder().https("http://").no("origin.invalid").build();
        assert!(validate_proxy_route(&uri, &matcher, "http://", "origin.invalid").is_ok());
        assert!(validate_proxy_route(&uri, &Matcher::builder().build(), "", "").is_ok());
    }

    #[test]
    fn invalid_specific_proxy_cannot_silently_fall_back_to_all_proxy() {
        let uri = "https://origin.invalid".parse().unwrap();
        for selected in ["ftp://bad.invalid", "http://", "not a proxy"] {
            let matcher = Matcher::builder().https(selected).all("http://fallback.invalid:8080").build();
            assert!(matcher.intercept(&uri).is_some());
            assert!(validate_proxy_route(&uri, &matcher, selected, "").is_err());
        }
    }

    #[test]
    fn no_proxy_wildcard_covers_domains_ipv4_and_ipv6() {
        for no_proxy in ["*", "localhost, *,example.org"] {
            let matcher = normalize_no_proxy(Matcher::builder().https("http://proxy.invalid:8080").no(no_proxy).build(), no_proxy);
            for url in ["https://origin.invalid", "https://127.0.0.1", "https://[::1]"] {
                let uri = url.parse().unwrap();
                assert!(matcher.intercept(&uri).is_none());
                assert!(validate_proxy_route(&uri, &matcher, "http://proxy.invalid:8080", no_proxy).is_ok());
            }
        }
    }

    #[tokio::test]
    async fn direct_and_no_proxy_paths_still_work() {
        for bypass in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let task = tokio::spawn(async move { grpc_unauthenticated(listener.accept().await.unwrap().0).await });
            let matcher = if bypass { Matcher::builder().all("http://127.0.0.1:9").no("127.0.0.1").build() }
                else { Matcher::builder().build() };
            let channel = connect_with_matcher(rc_core::transport::endpoint(&format!("http://{addr}")).unwrap(), matcher).await.unwrap();
            assert_rpc_reaches_server(channel).await;
            task.abort();
        }
    }

    #[tokio::test]
    async fn connect_proxy_resolves_origin_and_preserves_grpc() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = connect_request(&mut stream).await;
            assert!(request.starts_with("CONNECT origin.invalid:80 HTTP/1.1\r\n"));
            stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
            grpc_unauthenticated(stream).await;
        });
        let matcher = Matcher::builder().all(format!("http://{addr}")).build();
        let channel = connect_with_matcher(rc_core::transport::endpoint("http://origin.invalid").unwrap(), matcher).await.unwrap();
        assert_rpc_reaches_server(channel).await;
        task.abort();
    }

    #[tokio::test]
    async fn rejected_proxy_does_not_fall_back_or_leak_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = connect_request(&mut stream).await;
            assert!(request.to_lowercase().contains("proxy-authorization: basic cHJveHl1c2VyOnByb3h5cGFzcw==".to_lowercase().as_str()));
            stream.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        });
        let matcher = Matcher::builder().all(format!("http://proxyuser:proxypass@{addr}")).build();
        let error = connect_with_matcher(rc_core::transport::endpoint("https://origin.invalid").unwrap(), matcher).await.unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("CONNECT proxy"));
        assert!(!error.contains("proxyuser"));
        assert!(!error.contains("proxypass"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn unsupported_proxy_never_silently_falls_back() {
        let matcher = Matcher::builder().all("socks5://127.0.0.1:9").build();
        let error = connect_with_matcher(rc_core::transport::endpoint("https://origin.invalid").unwrap(), matcher).await.unwrap_err();
        assert!(error.to_string().contains("unsupported"));
    }

    #[tokio::test]
    async fn tunnel_accepts_fragmented_headers_and_preserves_early_payload() {
        let (client, mut proxy) = tokio::io::duplex(16384);
        let task = tokio::spawn(async move {
            connect_request(&mut proxy).await;
            proxy.write_all(b"H").await.unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
            proxy.write_all(b"TTP/1.1 200 OK\r\nX-Test: yes\r\n\r\nearly").await.unwrap();
        });
        let mut stream = establish_tunnel(client, &"http://origin.invalid:80".parse().unwrap(), None).await.unwrap();
        let mut payload = [0; 5];
        stream.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"early");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn tunnel_rejects_truncated_malformed_and_oversized_headers() {
        for response in [b"HTTP/1.1 200 OK\r\n".to_vec(), b"invalid\r\n\r\n".to_vec(),
            [b"HTTP/1.1 200 OK\r\nX-Test: ".as_slice(), &vec![b'x'; 8192]].concat()] {
            let (client, mut proxy) = tokio::io::duplex(32768);
            let task = tokio::spawn(async move {
                connect_request(&mut proxy).await;
                proxy.write_all(&response).await.unwrap();
                proxy.shutdown().await.unwrap();
            });
            let result = establish_tunnel(client, &"http://origin.invalid:80".parse().unwrap(), None).await;
            assert!(result.is_err());
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn untrusted_https_proxy_certificate_is_rejected() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let key = tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
        let tls = tokio_rustls::rustls::ServerConfig::builder().with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key.into()).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            assert!(acceptor.accept(listener.accept().await.unwrap().0).await.is_err());
        });
        let matcher = Matcher::builder().all(format!("https://{addr}")).build();
        let error = connect_with_matcher(rc_core::transport::endpoint("https://origin.invalid").unwrap(), matcher).await.unwrap_err();
        assert!(format!("{error:#}").contains("certificate"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn untrusted_destination_certificate_is_rejected_through_proxy() {
        let cert = rcgen::generate_simple_self_signed(vec!["origin.invalid".to_string()]).unwrap();
        let key = tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
        let mut tls = tokio_rustls::rustls::ServerConfig::builder().with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key.into()).unwrap();
        tls.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            connect_request(&mut stream).await;
            stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
            assert!(acceptor.accept(stream).await.is_err());
        });
        let matcher = Matcher::builder().all(format!("http://{addr}")).build();
        let error = connect_with_matcher(rc_core::transport::endpoint("https://origin.invalid").unwrap(), matcher).await.unwrap_err();
        assert!(format!("{error:#}").contains("certificate"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn trusted_tunnel_succeeds_but_wrong_origin_hostname_is_rejected() {
        for valid_name in [true, false] {
            let name = if valid_name { "origin.invalid" } else { "wrong.invalid" };
            let cert = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
            let trusted_cert = tonic::transport::Certificate::from_pem(cert.cert.pem());
            let key = tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
            let mut tls = tokio_rustls::rustls::ServerConfig::builder().with_no_client_auth()
                .with_single_cert(vec![cert.cert.der().clone()], key.into()).unwrap();
            tls.alpn_protocols = vec![b"h2".to_vec()];
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                connect_request(&mut stream).await;
                stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
                if let Ok(stream) = acceptor.accept(stream).await {
                    grpc_unauthenticated(stream).await;
                }
            });
            let endpoint = Endpoint::from_static("https://origin.invalid")
                .tls_config(tonic::transport::ClientTlsConfig::new().ca_certificate(trusted_cert)).unwrap();
            let matcher = Matcher::builder().all(format!("http://{addr}")).build();
            let result = connect_with_matcher(endpoint, matcher).await;
            if valid_name {
                assert_rpc_reaches_server(result.unwrap()).await;
                task.abort();
            } else {
                let error = format!("{:#}", result.unwrap_err()).to_lowercase();
                assert!(error.contains("certificate") && error.contains("name"), "{error}");
                task.await.unwrap();
            }
        }
    }
}
