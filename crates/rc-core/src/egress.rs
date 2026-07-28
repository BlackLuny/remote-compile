//! Egress host patterns (§7.1).
//!
//! The build sandbox has no route to anywhere except the worker's proxy, and
//! the proxy answers only for hosts on an allowlist. A repository may ask for
//! more than the fleet default — an internal registry, a private git host — and
//! an administrator decides.
//!
//! What is validated here is the *shape* of a request, and it is validated in
//! the agent (so a typo is a config error, not a mystery 403) and again in the
//! control plane (which trusts no agent). The rules exist because an allowlist
//! entry is a hole in the sandbox: §16 is explicit that any reachable host is a
//! channel a malicious `build.rs` can encode data into, so `*` and `*.com` must
//! not be things an administrator can approve by misreading one line.
//!
//! ## What a host name means here
//!
//! The proxy dials from the **worker host's** network namespace, not from the
//! container. So `localhost` does not mean the build's own loopback — it means
//! every service bound to the worker's loopback, and `169.254.169.254` means
//! cloud instance metadata. Those are the entries an administrator is *most*
//! likely to wave through, because they read like a local dev registry.
//!
//! This module therefore refuses anything that is an address rather than a
//! name. That is necessary but not sufficient: `127.0.0.1.nip.io` is a perfectly
//! ordinary name that resolves to loopback, and no amount of string inspection
//! catches it. The guarantee lives at dial time, in the worker's proxy, which
//! refuses to connect to a loopback/private/link-local address whatever name
//! pointed there. Both layers exist on purpose: this one turns a bad request
//! into a legible config error, that one is the actual boundary.

/// Normalise a requested host pattern, or say why it cannot be one.
///
/// Accepts `example.com` and `*.example.com`. The wildcard covers subdomains
/// and the bare domain, matching what the worker's proxy already implements.
pub fn normalize(pattern: &str) -> Result<String, String> {
    let host = pattern.trim().to_ascii_lowercase();
    if host.is_empty() {
        return Err("an egress entry cannot be empty".into());
    }
    // A URL is the commonest mistake: the proxy matches host names, and
    // `https://example.com/path` would silently match nothing.
    for bad in ["://", "/", "?", "#", "@", " "] {
        if host.contains(bad) {
            return Err(format!(
                "`{pattern}` looks like a URL; egress entries are host names, e.g. \
                 `example.com` or `*.example.com`"
            ));
        }
    }
    if host.contains(':') {
        return Err(format!(
            "`{pattern}` carries a port; the proxy matches host names only, and it \
             dials 80 and 443 and nothing else"
        ));
    }
    let labels: Vec<&str> = match host.strip_prefix("*.") {
        Some(rest) => {
            if rest.contains('*') {
                return Err(format!("`{pattern}`: only a leading `*.` wildcard is supported"));
            }
            rest.split('.').collect()
        }
        None => {
            if host.contains('*') {
                return Err(format!(
                    "`{pattern}`: a wildcard is only allowed as a leading `*.`, which covers \
                     subdomains — `*.example.com`"
                ));
            }
            host.split('.').collect()
        }
    };
    if labels.iter().any(|l| l.is_empty()) {
        return Err(format!("`{pattern}` has an empty label"));
    }
    if !labels
        .iter()
        .all(|l| l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    {
        return Err(format!(
            "`{pattern}` is not a host name: labels may hold letters, digits and `-`"
        ));
    }
    // `*.com` would put every site under a public suffix inside the sandbox on
    // the strength of one approval click. Two labels is not a real guarantee —
    // `*.co.uk` gets through — but it stops the mistake that costs the most.
    if host.starts_with("*.") && labels.len() < 2 {
        return Err(format!(
            "`{pattern}` is too broad: a wildcard needs a domain under the public suffix, \
             e.g. `*.example.com`"
        ));
    }
    if labels.len() < 2 {
        // `localhost` used to be admitted here. It named the *worker's*
        // loopback, not the build's, which is the opposite of what anyone
        // approving it would have assumed.
        return Err(format!("`{pattern}` is not a fully qualified host name"));
    }
    // An address is not a name. Dotted-quad (`10.0.0.1`), shorthand (`127.1`)
    // and hex (`0x7f.0.0.1`) all satisfy the label rules above and all dial the
    // worker host's own network. A real top-level domain always contains a
    // letter, so that is the discriminator.
    // The last label must *begin* with a letter, not merely contain one:
    // `inet_aton` accepts a hex final part, so `127.0.0.0x1` is loopback while
    // its last label `0x1` does contain a letter. Every real top-level domain
    // starts with one, punycode (`xn--p1ai`) included.
    let tld = labels.last().copied().unwrap_or("");
    if !tld.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return Err(format!(
            "`{pattern}` is an IP address, not a host name; the proxy dials from the worker, \
             so an address here reaches the worker's own network rather than the build's"
        ));
    }
    Ok(host)
}

/// Normalise a whole list, reporting every problem rather than the first: a
/// config with three typos should take one round trip to fix, not three.
pub fn normalize_all(patterns: &[String]) -> Result<Vec<String>, Vec<String>> {
    let mut ok = Vec::new();
    let mut errors = Vec::new();
    for p in patterns {
        match normalize(p) {
            Ok(host) => ok.push(host),
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        ok.sort();
        ok.dedup();
        Ok(ok)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_hosts_and_subdomain_wildcards_are_accepted() {
        assert_eq!(normalize("example.com").unwrap(), "example.com");
        assert_eq!(normalize("*.example.com").unwrap(), "*.example.com");
        assert_eq!(normalize("  Registry.Corp.Internal  ").unwrap(), "registry.corp.internal");
    }

    #[test]
    fn the_patterns_that_would_open_the_sandbox_are_refused() {
        for pattern in ["*", "*.com", "*.*", "*.example.*", "example.*"] {
            assert!(normalize(pattern).is_err(), "`{pattern}` must not be accepted");
        }
    }

    #[test]
    fn an_address_is_not_a_name_however_it_is_spelled() {
        // Every one of these dials the worker host's own network, and every one
        // of them satisfies the label rules. `127.1` and `0x7f.0.0.1` are the
        // ones that look least like an address to a human reading an approval
        // queue — inet_aton resolves both to 127.0.0.1.
        for pattern in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "172.17.0.1",
            "127.1",
            "0x7f.0.0.1",
            // A hex final part: `inet_aton` reads this as 127.0.0.1, and the
            // label `0x1` is not digits-only, so a laxer rule lets it through.
            "127.0.0.0x1",
            "0x7f.0.0.0x1",
            "*.1.0.1",
        ] {
            let err = normalize(pattern).unwrap_err();
            assert!(err.contains("IP address"), "`{pattern}`: {err}");
        }
    }

    #[test]
    fn ordinary_names_survive_the_address_rule() {
        // The rule looks at the last label only, so a digit-leading label
        // anywhere else is fine — and a punycode TLD starts with a letter.
        for pattern in ["3com.example.com", "1.2.3.example.com", "x.xn--p1ai", "*.4chan.org"] {
            assert!(normalize(pattern).is_ok(), "`{pattern}` must still be accepted");
        }
    }

    #[test]
    fn localhost_is_refused_because_it_is_the_workers_loopback_not_the_builds() {
        assert!(normalize("localhost").is_err());
    }

    #[test]
    fn a_url_is_reported_as_a_url_rather_than_matching_nothing() {
        let err = normalize("https://example.com/index").unwrap_err();
        assert!(err.contains("host names"), "{err}");
        assert!(normalize("example.com:8443").unwrap_err().contains("port"));
    }

    #[test]
    fn every_bad_entry_is_reported_at_once() {
        let errs = normalize_all(&["ok.example.com".into(), "*".into(), "https://x/y".into()])
            .unwrap_err();
        assert_eq!(errs.len(), 2, "{errs:?}");
    }

    #[test]
    fn a_valid_list_comes_back_sorted_and_deduplicated() {
        let hosts = normalize_all(&[
            "b.example.com".into(),
            "a.example.com".into(),
            "A.example.com".into(),
        ])
        .unwrap();
        assert_eq!(hosts, vec!["a.example.com", "b.example.com"]);
    }
}
