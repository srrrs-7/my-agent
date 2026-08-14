//! URL admission policy for the web fetcher.
//!
//! The URL is model-supplied and untrusted; this module decides whether the
//! process may talk to it at all. Two layers, both of which must pass:
//!
//! 1. **Lexical** ([`check_url`]): scheme, userinfo, host *shape* - IP
//!    literals in non-global ranges, `localhost`, dotless single-label names
//!    (container DNS like `ollama`), `.internal`/`.local` suffixes and
//!    `host.docker.internal` are refused without touching the network.
//! 2. **Resolved addresses** ([`check_resolved_addrs`]): every IP the hostname
//!    resolves to must be globally routable, so a public-looking name cannot
//!    smuggle a connection to `169.254.169.254` or the compose network.
//!
//! Residual risk, accepted for now: a DNS-rebinding server could answer our
//! resolution check with a public address and the actual connection with a
//! private one (the fetch re-resolves). Closing that requires pinning the
//! connection to the checked address; noted in the issue as out of scope.

use std::net::IpAddr;

use agent_domain::error::FetchError;
use url::Url;

/// Non-DNS validation. Returns the parsed URL so callers cannot forget to
/// parse with the same rules they validated with.
pub(crate) fn check_url(raw: &str, allow_private: bool) -> Result<Url, FetchError> {
    let url = Url::parse(raw).map_err(|error| FetchError::InvalidUrl {
        url: raw.to_string(),
        reason: error.to_string(),
    })?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(FetchError::Blocked {
            url: raw.to_string(),
            reason: format!(
                "scheme `{}` is not allowed, only http and https",
                url.scheme()
            ),
        });
    }

    // `http://user@host/` is a classic confusion vector; nothing legitimate
    // the agent fetches needs credentials in the URL.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FetchError::Blocked {
            url: raw.to_string(),
            reason: "URLs with embedded credentials are not allowed".to_string(),
        });
    }

    let Some(host) = url.host() else {
        return Err(FetchError::InvalidUrl {
            url: raw.to_string(),
            reason: "the URL has no host".to_string(),
        });
    };

    if !allow_private {
        match host {
            url::Host::Ipv4(ip) => check_ip(raw, IpAddr::V4(ip))?,
            url::Host::Ipv6(ip) => check_ip(raw, IpAddr::V6(ip))?,
            url::Host::Domain(name) => check_hostname(raw, name)?,
        }
    }

    Ok(url)
}

/// DNS-resolution check: refuses the URL if *any* resolved address is
/// non-global. Callers resolve; this stays pure and testable.
pub(crate) fn check_resolved_addrs(
    raw: &str,
    addrs: impl IntoIterator<Item = IpAddr>,
) -> Result<(), FetchError> {
    let mut any = false;
    for addr in addrs {
        any = true;
        check_ip(raw, addr)?;
    }
    if !any {
        return Err(FetchError::Transport {
            url: raw.to_string(),
            message: "the hostname did not resolve to any address".to_string(),
        });
    }
    Ok(())
}

fn check_hostname(raw: &str, name: &str) -> Result<(), FetchError> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();

    let blocked_reason = if name == "localhost" || name.ends_with(".localhost") {
        Some("`localhost` is the container itself")
    } else if name == "host.docker.internal" || name == "gateway.docker.internal" {
        Some("it points at the container host")
    } else if name.ends_with(".internal") || name.ends_with(".local") {
        Some("`.internal` / `.local` names are private")
    } else if !name.contains('.') {
        // `ollama`, `db`, `gateway` - single-label names are container/LAN DNS.
        Some("single-label hostnames resolve on the private network")
    } else {
        None
    };

    match blocked_reason {
        Some(reason) => Err(FetchError::Blocked {
            url: raw.to_string(),
            reason: format!("host `{name}` is not public: {reason}"),
        }),
        None => Ok(()),
    }
}

fn check_ip(raw: &str, addr: IpAddr) -> Result<(), FetchError> {
    if is_global(addr) {
        Ok(())
    } else {
        Err(FetchError::Blocked {
            url: raw.to_string(),
            reason: format!("address {addr} is private, loopback or link-local"),
        })
    }
}

/// Conservative "globally routable" test (stable-Rust stand-in for the
/// unstable `IpAddr::is_global`). Anything not provably public is refused.
fn is_global(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            !(ip.is_loopback()                  // 127.0.0.0/8
                || ip.is_private()              // 10/8, 172.16/12, 192.168/16
                || ip.is_link_local()           // 169.254/16 (cloud metadata!)
                || ip.is_unspecified()          // 0.0.0.0
                || ip.is_broadcast()
                || ip.is_documentation()        // TEST-NET-1/2/3
                || octets[0] == 100 && (octets[1] & 0xC0) == 64      // CGNAT 100.64/10
                || octets[0] == 192 && octets[1] == 0 && octets[2] == 0 // 192.0.0.0/24
                || octets[0] == 198 && (octets[1] & 0xFE) == 18      // 198.18/15 benchmark
                || octets[0] >= 240) // 240/4 reserved
        }
        IpAddr::V6(ip) => {
            // Order matters. `::` and `::1` are IPv4-compatible in *form*, so
            // decoding them first would yield 0.0.0.0 / 0.0.0.1 - and 0.0.0.1
            // looks perfectly routable.
            if ip.is_loopback() || ip.is_unspecified() {
                return false;
            }
            // Several IPv6 forms carry an IPv4 address inside them. Judging the
            // embedded address is what stops them from smuggling a private
            // destination past a v6-shaped check.
            if let Some(v4) = embedded_ipv4(ip.segments()) {
                return is_global(IpAddr::V4(v4));
            }
            let segments = ip.segments();
            !((segments[0] & 0xfe00) == 0xfc00        // fc00::/7 unique local
                || (segments[0] & 0xffc0) == 0xfe80   // fe80::/10 link-local
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)) // 2001:db8::/32 docs
        }
    }
}

/// The IPv4 address an IPv6 address embeds, if any.
///
/// Callers must rule out `::` and `::1` first (see [`is_global`]).
///
/// Decoding rather than blocking these prefixes outright is deliberate: on an
/// IPv6-only network with DNS64, a perfectly legitimate hostname *resolves* to
/// `64:ff9b::<public v4>`, so refusing the whole prefix would break normal use.
/// Decoding admits that address and still refuses `64:ff9b::a9fe:a9fe`.
fn embedded_ipv4(segments: [u16; 8]) -> Option<std::net::Ipv4Addr> {
    let v4 = |high: u16, low: u16| {
        std::net::Ipv4Addr::new((high >> 8) as u8, high as u8, (low >> 8) as u8, low as u8)
    };
    match segments {
        // ::ffff:a.b.c.d (IPv4-mapped) and ::a.b.c.d (IPv4-compatible)
        [0, 0, 0, 0, 0, 0xffff | 0, high, low] => Some(v4(high, low)),
        // 64:ff9b::a.b.c.d - NAT64 well-known prefix (RFC 6052)
        [0x0064, 0xff9b, ..] => Some(v4(segments[6], segments[7])),
        // 2002:a.b.c.d:: - 6to4 (RFC 3056)
        [0x2002, high, low, ..] => Some(v4(high, low)),
        // 2001:0::/32 - Teredo (RFC 4380); the client v4 is the inverted tail
        [0x2001, 0, ..] => Some(v4(!segments[6], !segments[7])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(raw: &str) {
        let error = check_url(raw, false).expect_err(raw);
        assert!(
            matches!(
                error,
                FetchError::Blocked { .. } | FetchError::InvalidUrl { .. }
            ),
            "{raw} must be refused, got {error:?}"
        );
    }

    fn allowed(raw: &str) {
        check_url(raw, false).unwrap_or_else(|error| panic!("{raw} must pass, got {error:?}"));
    }

    #[test]
    fn public_hosts_pass() {
        allowed("https://docs.rs/serde");
        allowed("https://doc.rust-lang.org/std/");
        allowed("http://example.com:8080/path?q=1");
        allowed("https://93.184.216.34/"); // public IP literal
    }

    #[test]
    fn non_http_schemes_are_blocked() {
        blocked("file:///etc/passwd");
        blocked("ftp://example.com/");
        blocked("gopher://example.com/");
    }

    #[test]
    fn embedded_credentials_are_blocked() {
        blocked("https://user@example.com/");
        blocked("https://user:pass@example.com/");
    }

    #[test]
    fn loopback_and_private_ipv4_are_blocked() {
        blocked("http://127.0.0.1:11434/");
        blocked("http://10.0.0.1/");
        blocked("http://172.16.0.1/");
        blocked("http://172.31.255.255/");
        blocked("http://192.168.1.1/");
        blocked("http://0.0.0.0/");
        blocked("http://100.64.0.1/"); // CGNAT
    }

    #[test]
    fn the_cloud_metadata_endpoint_is_blocked() {
        blocked("http://169.254.169.254/latest/meta-data/");
    }

    #[test]
    fn private_ipv6_is_blocked() {
        blocked("http://[::1]/");
        blocked("http://[fc00::1]/");
        blocked("http://[fd12:3456::1]/");
        blocked("http://[fe80::1]/");
        blocked("http://[2001:db8::1]/"); // documentation range
        blocked("http://[::ffff:127.0.0.1]/"); // IPv4-mapped loopback
        blocked("http://[::ffff:10.0.0.1]/"); // IPv4-mapped private
    }

    /// Several IPv6 forms carry an IPv4 address inside them. Each is the only
    /// gate for an IP *literal* (literals skip the DNS re-check), and DNS64
    /// makes them reachable through hostname resolution too.
    #[test]
    fn ipv6_forms_embedding_a_private_ipv4_are_blocked() {
        // NAT64 (64:ff9b::/96) pointing at the cloud metadata endpoint and at
        // loopback.
        blocked("http://[64:ff9b::a9fe:a9fe]/latest/meta-data/"); // 169.254.169.254
        blocked("http://[64:ff9b::7f00:1]/"); // 127.0.0.1
        blocked("http://[64:ff9b::c0a8:1]/"); // 192.168.0.1

        // 6to4 (2002::/16).
        blocked("http://[2002:c0a8:0101::]/"); // 192.168.1.1
        blocked("http://[2002:a9fe:a9fe::]/"); // 169.254.169.254

        // IPv4-compatible (::a.b.c.d), which `to_ipv4_mapped` does not decode.
        blocked("http://[::10.0.0.1]/");
        blocked("http://[::a00:1]/"); // same address, hex form

        // Teredo (2001:0::/32): the client v4 is the inverted tail, so
        // 5601:5601 decodes to 169.254.169.254.
        blocked("http://[2001::5601:5601]/");
    }

    #[test]
    fn ipv6_forms_embedding_a_public_ipv4_still_pass() {
        // The DNS64 case: an IPv6-only network resolves a public host to a
        // NAT64 address, and that must keep working.
        allowed("http://[64:ff9b::5db8:d822]/"); // 93.184.216.34
        allowed("http://[2002:5db8:d822::]/"); // 6to4 for the same address
        allowed("http://[2606:4700:4700::1111]/"); // an ordinary public v6
    }

    #[test]
    fn reserved_ipv4_ranges_are_blocked() {
        blocked("http://192.0.0.1/"); // 192.0.0.0/24, IETF assignments
        blocked("http://198.18.0.1/"); // 198.18.0.0/15, benchmarking
        blocked("http://198.19.255.255/");
        blocked("http://240.0.0.1/"); // 240.0.0.0/4, reserved
        blocked("http://192.0.2.1/"); // TEST-NET-1
    }

    #[test]
    fn container_and_internal_hostnames_are_blocked() {
        blocked("http://localhost:11434/");
        blocked("http://sub.localhost/");
        blocked("http://ollama:11434/"); // single-label container DNS
        blocked("http://host.docker.internal:11434/");
        blocked("http://metadata.internal/");
        blocked("http://printer.local/");
    }

    #[test]
    fn allow_private_lifts_the_address_policy_but_not_scheme_rules() {
        check_url("http://127.0.0.1:8080/", true).unwrap();
        check_url("http://ollama:11434/", true).unwrap();
        // Scheme and credential rules always apply.
        assert!(check_url("file:///etc/passwd", true).is_err());
        assert!(check_url("https://user@example.com/", true).is_err());
    }

    #[test]
    fn resolved_addresses_are_all_checked() {
        use std::net::{Ipv4Addr, Ipv6Addr};

        check_resolved_addrs(
            "https://example.com/",
            [IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
        )
        .unwrap();

        // One private record among public ones poisons the lot.
        let error = check_resolved_addrs(
            "https://rebind.example/",
            [
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            ],
        )
        .expect_err("must refuse");
        assert!(matches!(error, FetchError::Blocked { .. }));

        let error = check_resolved_addrs("https://v6.example/", [IpAddr::V6(Ipv6Addr::LOCALHOST)])
            .expect_err("must refuse");
        assert!(matches!(error, FetchError::Blocked { .. }));

        // DNS64 hands back NAT64 addresses; a private one behind that prefix
        // must be refused exactly like the bare IPv4 would be.
        let error = check_resolved_addrs(
            "https://dns64.example/",
            ["64:ff9b::a9fe:a9fe".parse::<Ipv6Addr>().unwrap().into()],
        )
        .expect_err("must refuse");
        assert!(matches!(error, FetchError::Blocked { .. }));

        assert!(check_resolved_addrs("https://none.example/", []).is_err());
    }
}
