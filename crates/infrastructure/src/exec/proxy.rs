//! The egress allowlist, as an in-process HTTP proxy.
//!
//! ## Why a proxy is not a design choice
//!
//! Landlock expresses network rules over *ports* and nothing else - the
//! kernel's `landlock_net_port_attr` has no host field. A domain allowlist is
//! therefore inexpressible at the syscall layer, and the only way to recover
//! it is to make the single reachable port speak a protocol that carries the
//! destination name. That protocol is HTTP proxying. This is also why the
//! reference agents ship a proxy rather than relying on their sandbox alone.
//!
//! ## Why in-process
//!
//! Spawning `socat` or a sidecar would reintroduce the install step this whole
//! design exists to avoid (see [`super`]). A tokio listener on loopback costs
//! one bound port and keeps the binary self-contained.
//!
//! ## What it enforces
//!
//! Both proxy shapes a client can use are checked against the same
//! [`HostPolicy`] as `web_fetch`, so there is one allowlist and one set of
//! rules to get right:
//!
//! * `CONNECT host:port` - the HTTPS path, and the one that matters, since
//!   TLS means the tunnel is opaque afterwards. The name is checked before a
//!   byte of TLS flows.
//! * `GET http://host/path` (absolute-form) - the plaintext path. Rewritten to
//!   origin-form before forwarding, since the origin server does not speak
//!   proxy.
//!
//! Names are checked lexically *and* by resolved address, and the connection
//! goes to an address that was actually validated rather than to a re-resolved
//! one.
//!
//! ## Residual gaps, stated rather than papered over
//!
//! * The proxy is opt-in for the child: a program that ignores `HTTP_PROXY`
//!   simply fails to connect, because Landlock blocks every other port. It
//!   cannot escape, but its error message will be a connection refusal rather
//!   than a policy message.
//! * A child may open the proxy's port on *some other loopback service*
//!   sharing that port number. Nothing else listens there in practice, and
//!   closing it properly needs the network-namespace tier.
//! * `CONNECT` is checked by name; a client that lies about the SNI it will
//!   send afterwards reaches a host we allowed by name anyway. Inspecting TLS
//!   would mean terminating it, which is a larger trade than it is worth here.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream, lookup_host};
use tokio::task::JoinHandle;
use tracing::debug;
use url::Host;

use crate::net::guard::HostPolicy;

/// Cap on the request head we buffer. Generous for real headers, small enough
/// that a client cannot make us hold memory by never sending the blank line.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Give up on the accept loop after this many consecutive failures, rather
/// than spinning forever on a condition that is not going to clear.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: usize = 16;

/// How long a client gets to finish its request head.
///
/// Without this a connection that dribbles bytes - or sends none at all - pins
/// a task for as long as it likes. The relay that follows is deliberately not
/// bounded: a long download is legitimate, and the command's own timeout is
/// what ends it.
const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A running proxy. Dropping it closes the port.
pub(crate) struct EgressProxy {
    port: u16,
    accept_loop: JoinHandle<()>,
}

impl EgressProxy {
    /// Binds an ephemeral loopback port and starts serving.
    ///
    /// Loopback-only: the allowlist is for the child of this process, and a
    /// proxy reachable from the network would be an open relay.
    pub(crate) async fn start(policy: HostPolicy) -> std::io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
        let port = listener.local_addr()?.port();
        let policy = Arc::new(policy);

        let accept_loop = tokio::spawn(async move {
            let mut failures = 0_usize;
            loop {
                match listener.accept().await {
                    Ok((client, _)) => {
                        failures = 0;
                        let policy = Arc::clone(&policy);
                        tokio::spawn(async move {
                            if let Err(error) = serve(client, &policy).await {
                                debug!(%error, "an egress proxy connection ended early");
                            }
                        });
                    }
                    Err(error) => {
                        failures += 1;
                        debug!(%error, failures, "the egress proxy failed to accept");
                        if failures >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self { port, accept_loop })
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        // Stops accepting and releases the port. Connections already being
        // relayed finish on their own; they outlive the proxy only until their
        // peers close, and the process is on its way out by then.
        self.accept_loop.abort();
    }
}

/// Handles one client connection start to finish.
async fn serve(mut client: TcpStream, policy: &HostPolicy) -> std::io::Result<()> {
    let (head, leftover) = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client))
        .await
        .map_err(|_| {
            std::io::Error::other("the client did not finish its request head in time")
        })??;
    let head = String::from_utf8_lossy(&head).into_owned();

    let request = match parse_request(&head) {
        Ok(request) => request,
        Err(reason) => return refuse(&mut client, "400 Bad Request", &reason).await,
    };

    let checked = match check(policy, &request) {
        Ok(checked) => checked,
        Err(reason) => return refuse(&mut client, "403 Forbidden", &reason).await,
    };

    let mut upstream = match connect(policy, &checked).await {
        Ok(upstream) => upstream,
        Err(reason) => return refuse(&mut client, "502 Bad Gateway", &reason).await,
    };

    match &request.kind {
        RequestKind::Tunnel => {
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;
        }
        RequestKind::Forward { origin_form } => {
            let rewritten = rewrite_head(&request, origin_form);
            upstream.write_all(rewritten.as_bytes()).await?;
        }
    }

    // Whatever arrived in the same read as the head is already client data.
    if !leftover.is_empty() {
        upstream.write_all(&leftover).await?;
    }

    copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Reads up to and including the blank line, returning the head and any bytes
/// that arrived behind it.
///
/// Splitting the two matters: a `CONNECT` client starts its TLS handshake
/// immediately, and those bytes routinely land in the same read as the head.
/// Dropping them would hang the tunnel.
async fn read_head(client: &mut TcpStream) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];

    loop {
        if let Some(end) = find_head_end(&buffer) {
            let leftover = buffer.split_off(end);
            return Ok((buffer, leftover));
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(std::io::Error::other(
                "the proxy request head exceeded the limit",
            ));
        }
        let read = client.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::other(
                "the client closed before sending a complete request",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Offset just past the `\r\n\r\n` that ends the head.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|start| start + 4)
}

#[derive(Debug, PartialEq, Eq)]
enum RequestKind {
    /// `CONNECT host:port` - relay bytes once the name is approved.
    Tunnel,
    /// Absolute-form request; carries the origin-form target to forward with.
    Forward { origin_form: String },
}

#[derive(Debug)]
struct ProxyRequest<'a> {
    method: &'a str,
    version: &'a str,
    kind: RequestKind,
    /// The request target exactly as the client wrote it. Kept so the policy
    /// sees the same string the client sent rather than one this module
    /// re-serialised - a URL that survives a parse-and-rebuild round trip is
    /// not necessarily the URL that was checked.
    raw_target: &'a str,
    /// Host as the policy wants to see it: IPv6 literals bracketed.
    policy_host: String,
    port: u16,
    header_block: &'a str,
}

impl ProxyRequest<'_> {
    /// `host` or `host:port`, omitting the port only when it is the scheme's
    /// default - the form an origin server expects to see.
    fn authority(&self) -> String {
        if matches!(self.port, 80 | 443) {
            self.policy_host.clone()
        } else {
            format!("{}:{}", self.policy_host, self.port)
        }
    }
}

/// Splits a proxy request head into the parts the policy needs.
fn parse_request(head: &str) -> Result<ProxyRequest<'_>, String> {
    let (request_line, header_block) = head.split_once("\r\n").unwrap_or((head, ""));

    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("the request line is empty")?;
    let target = parts.next().ok_or("the request line has no target")?;
    let version = parts.next().unwrap_or("HTTP/1.1");

    if method.eq_ignore_ascii_case("CONNECT") {
        let (policy_host, port) = split_authority(target)?;
        return Ok(ProxyRequest {
            method,
            version,
            kind: RequestKind::Tunnel,
            raw_target: target,
            policy_host,
            port,
            header_block,
        });
    }

    // Anything else must be absolute-form: that is what makes it a *proxy*
    // request rather than one meant for us.
    let url = url::Url::parse(target)
        .map_err(|error| format!("`{target}` is not an absolute URL: {error}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| format!("`{target}` has no host"))?
        .to_string();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| format!("`{target}` has no usable port"))?;

    let mut origin_form = url.path().to_string();
    if let Some(query) = url.query() {
        origin_form.push('?');
        origin_form.push_str(query);
    }

    Ok(ProxyRequest {
        method,
        version,
        kind: RequestKind::Forward { origin_form },
        raw_target: target,
        policy_host: host,
        port,
        header_block,
    })
}

/// Splits a `CONNECT` authority, keeping IPv6 brackets so the host still
/// parses as a literal rather than as a very strange domain name.
fn split_authority(target: &str) -> Result<(String, u16), String> {
    if let Some(rest) = target.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("`{target}` has an unterminated IPv6 literal"))?;
        let port = tail
            .strip_prefix(':')
            .ok_or_else(|| format!("`{target}` must include a port"))?;
        let port = port
            .parse()
            .map_err(|_| format!("`{port}` is not a port number"))?;
        return Ok((format!("[{host}]"), port));
    }

    let (host, port) = target
        .rsplit_once(':')
        .ok_or_else(|| format!("`{target}` must include a port"))?;
    let port = port
        .parse()
        .map_err(|_| format!("`{port}` is not a port number"))?;
    Ok((host.to_string(), port))
}

/// The destination, once the policy has approved the name.
struct Approved {
    /// Unbracketed, as the resolver wants it.
    host: String,
    port: u16,
}

/// One policy, two entry points - matched to what the client actually sent.
///
/// The absolute-form path goes through `check_url` rather than `check_host`
/// because a full URL carries more that has to be refused: a scheme this proxy
/// has no business relaying, and credentials embedded in the authority. A
/// `CONNECT` target has neither, so it takes the host-only path.
fn check(policy: &HostPolicy, request: &ProxyRequest<'_>) -> Result<Approved, String> {
    let host = match request.kind {
        RequestKind::Tunnel => policy
            .check_host(&request.policy_host)
            .map_err(|rejection| rejection.reason)?,
        RequestKind::Forward { .. } => {
            let url = policy
                .check_url(request.raw_target)
                .map_err(|rejection| rejection.reason)?;
            url.host()
                .ok_or_else(|| format!("`{}` has no host", request.raw_target))?
                .to_owned()
        }
    };

    Ok(Approved {
        host: resolver_form(&host),
        port: request.port,
    })
}

/// `Host` serialises IPv6 with brackets; the resolver wants it without.
fn resolver_form(host: &Host<String>) -> String {
    match host {
        Host::Domain(domain) => domain.clone(),
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => ip.to_string(),
    }
}

/// Resolves, re-checks every address, and connects to one we checked.
///
/// Connecting to a validated `SocketAddr` rather than to `(host, port)` is the
/// point: handing the hostname back to the resolver would let the second
/// lookup return an address the first one never showed us.
async fn connect(policy: &HostPolicy, approved: &Approved) -> Result<TcpStream, String> {
    let addrs: Vec<SocketAddr> = lookup_host((approved.host.as_str(), approved.port))
        .await
        .map_err(|error| format!("cannot resolve `{}`: {error}", approved.host))?
        .collect();

    if policy.resolves_before_connect() {
        policy
            .check_addrs(addrs.iter().map(SocketAddr::ip))
            .map_err(|rejection| rejection.reason)?;
    }

    let mut last_error = String::from("the hostname did not resolve to any address");
    for addr in &addrs {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = format!("cannot connect to {addr}: {error}"),
        }
    }
    Err(last_error)
}

/// Turns an absolute-form request back into what an origin server expects.
///
/// Three things are rewritten rather than relayed:
///
/// * **`Host`** is replaced with the authority from the URL we checked. A
///   client that asked for an allowed host while sending
///   `Host: something-else` would otherwise reach a different virtual host on
///   that server than the one the allowlist approved.
/// * **Hop-by-hop and proxy-only headers** are dropped; they are addressed to
///   this proxy, not to the origin.
/// * **The connection closes** after one exchange. Keep-alive would let a
///   later request on the same socket reach a host this one was never checked
///   against.
fn rewrite_head(request: &ProxyRequest<'_>, origin_form: &str) -> String {
    let mut out = format!("{} {} {}\r\n", request.method, origin_form, request.version);

    for line in request.header_block.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        let name = line
            .split_once(':')
            .map(|(name, _)| name.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(
            name.as_str(),
            "host" | "proxy-connection" | "proxy-authorization" | "connection" | "keep-alive"
        ) {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }

    out.push_str(&format!("Host: {}\r\n", request.authority()));
    out.push_str("Connection: close\r\n\r\n");
    out
}

/// Answers with the reason, so the failure reaches the model as a policy
/// message rather than as an unexplained connection error.
async fn refuse(client: &mut TcpStream, status: &str, reason: &str) -> std::io::Result<()> {
    debug!(status, reason, "the egress proxy refused a destination");
    let body = format!("my-agent egress proxy refused this destination: {reason}\n");
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(response.as_bytes()).await?;
    client.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::guard::DomainScope;

    /// A policy that admits loopback, which the real ones never do. Testing
    /// the proxy end to end needs a server we control, and the alternative is
    /// to test it against the internet.
    fn loopback_policy(domains: &[&str]) -> HostPolicy {
        HostPolicy {
            allow_private: true,
            scope: DomainScope::Only(domains.iter().map(|d| d.to_string()).collect()),
        }
    }

    /// A one-shot HTTP server. Returns its port and the request it received.
    async fn echo_server() -> (u16, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (head, _) = tokio::time::timeout(PATIENCE, read_head(&mut stream))
                .await
                .expect("the proxy never forwarded a request")
                .unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi")
                .await
                .unwrap();
            let _ = stream.shutdown().await;
            String::from_utf8_lossy(&head).into_owned()
        });
        (port, handle)
    }

    /// Every read below is bounded. A proxy bug shows up as a *hang*, and a
    /// hung test is far harder to diagnose than a failing one - especially in
    /// CI, where it looks like an infrastructure problem rather than a bug.
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

    async fn connect_to_proxy(proxy_port: u16, request: &str) -> TcpStream {
        let mut stream = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, proxy_port)))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        stream
    }

    /// Sends a raw request and reads until the proxy closes. Only valid for
    /// exchanges that end on their own - a refusal, or a forwarded request
    /// whose origin answers and hangs up.
    async fn through_proxy(proxy_port: u16, request: &str) -> String {
        let mut stream = connect_to_proxy(proxy_port, request).await;
        let mut response = Vec::new();
        tokio::time::timeout(PATIENCE, stream.read_to_end(&mut response))
            .await
            .expect("the proxy never closed the connection")
            .unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    /// Reads only the response head.
    ///
    /// An accepted `CONNECT` deliberately does *not* close: it becomes a tunnel
    /// that stays open until one side hangs up. Reading to EOF there waits for
    /// a close that is never coming.
    async fn proxy_response_head(proxy_port: u16, request: &str) -> String {
        let mut stream = connect_to_proxy(proxy_port, request).await;
        let (head, _) = tokio::time::timeout(PATIENCE, read_head(&mut stream))
            .await
            .expect("the proxy never answered")
            .unwrap();
        String::from_utf8_lossy(&head).into_owned()
    }

    #[tokio::test]
    async fn a_listed_host_is_forwarded_and_rewritten_to_origin_form() {
        let (origin, served) = echo_server().await;
        let proxy = EgressProxy::start(loopback_policy(&["localhost"]))
            .await
            .unwrap();

        let response = through_proxy(
            proxy.port(),
            &format!(
                "GET http://localhost:{origin}/path?q=1 HTTP/1.1\r\n\
                 Host: localhost:{origin}\r\n\
                 Proxy-Connection: keep-alive\r\n\r\n"
            ),
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");

        let seen = served.await.unwrap();
        assert!(
            seen.starts_with("GET /path?q=1 HTTP/1.1"),
            "the origin must see origin-form, got: {seen}"
        );
        assert!(
            !seen.to_ascii_lowercase().contains("proxy-connection"),
            "proxy-only headers must not be forwarded: {seen}"
        );
    }

    #[tokio::test]
    async fn an_unlisted_host_is_refused_with_a_reason() {
        let proxy = EgressProxy::start(loopback_policy(&["localhost"]))
            .await
            .unwrap();

        let response = through_proxy(
            proxy.port(),
            "GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(
            response.contains("allowlist"),
            "the reason must say why: {response}"
        );
    }

    #[tokio::test]
    async fn connect_is_checked_before_the_tunnel_opens() {
        let (origin, served) = echo_server().await;
        let proxy = EgressProxy::start(loopback_policy(&["localhost"]))
            .await
            .unwrap();

        let allowed = proxy_response_head(
            proxy.port(),
            &format!("CONNECT localhost:{origin} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        )
        .await;
        assert!(
            allowed.starts_with("HTTP/1.1 200 Connection Established"),
            "{allowed}"
        );
        served.abort();

        let refused = through_proxy(
            proxy.port(),
            "CONNECT evil.example:443 HTTP/1.1\r\nHost: evil.example\r\n\r\n",
        )
        .await;
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    }

    #[tokio::test]
    async fn bytes_sent_with_the_connect_head_are_not_lost() {
        // A TLS client starts its handshake without waiting, so those bytes
        // arrive in the same read as the head. Losing them hangs the tunnel.
        let (origin, served) = echo_server().await;
        let proxy = EgressProxy::start(loopback_policy(&["localhost"]))
            .await
            .unwrap();

        let response = through_proxy(
            proxy.port(),
            &format!(
                "CONNECT localhost:{origin} HTTP/1.1\r\n\r\n\
                 GET /early HTTP/1.1\r\nHost: localhost\r\n\r\n"
            ),
        )
        .await;

        assert!(
            response.contains("200 Connection Established"),
            "{response}"
        );
        let seen = served.await.unwrap();
        assert!(
            seen.starts_with("GET /early"),
            "the early bytes must reach the origin, got: {seen}"
        );
    }

    #[tokio::test]
    async fn a_scheme_this_proxy_does_not_relay_is_refused() {
        // The host is on the allowlist; the scheme is the only thing wrong,
        // and only the URL-shaped check can see it.
        let proxy = EgressProxy::start(loopback_policy(&["localhost"]))
            .await
            .unwrap();

        let response = through_proxy(
            proxy.port(),
            "GET ftp://localhost/secret HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(response.contains("scheme"), "{response}");
    }

    #[tokio::test]
    async fn credentials_in_the_target_url_are_refused() {
        let proxy = EgressProxy::start(loopback_policy(&["localhost"]))
            .await
            .unwrap();

        let response = through_proxy(
            proxy.port(),
            "GET http://user:pw@localhost/ HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(response.contains("credentials"), "{response}");
    }

    #[tokio::test]
    async fn an_ip_literal_cannot_sidestep_the_domain_allowlist() {
        let (origin, _served) = echo_server().await;
        let proxy = EgressProxy::start(loopback_policy(&["localhost"]))
            .await
            .unwrap();

        let response = through_proxy(
            proxy.port(),
            &format!("CONNECT 127.0.0.1:{origin} HTTP/1.1\r\n\r\n"),
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    }

    #[tokio::test]
    async fn the_default_policy_admits_nothing() {
        let proxy = EgressProxy::start(HostPolicy::allowing(Vec::new()))
            .await
            .unwrap();

        for request in [
            "GET http://example.com/ HTTP/1.1\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\n\r\n",
        ] {
            let response = through_proxy(proxy.port(), request).await;
            assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        }
    }

    #[tokio::test]
    async fn a_private_address_behind_a_listed_name_is_still_refused() {
        // The lexical check passes - `localhost` is on the list - so only the
        // resolved-address check can catch this one.
        let proxy = EgressProxy::start(HostPolicy {
            allow_private: false,
            scope: DomainScope::Only(vec!["localhost".to_string()]),
        })
        .await
        .unwrap();

        let response = through_proxy(proxy.port(), "CONNECT localhost:443 HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    }

    #[tokio::test]
    async fn a_request_that_is_not_proxy_shaped_is_a_client_error() {
        let proxy = EgressProxy::start(loopback_policy(&["localhost"]))
            .await
            .unwrap();

        // Origin-form: meaningful to a server, meaningless to a proxy.
        let response = through_proxy(proxy.port(), "GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    }

    #[test]
    fn authority_parsing_keeps_ipv6_recognisable() {
        assert_eq!(
            split_authority("[2001:db8::1]:443").unwrap(),
            ("[2001:db8::1]".to_string(), 443)
        );
        assert_eq!(
            split_authority("example.com:8443").unwrap(),
            ("example.com".to_string(), 8443)
        );
        assert!(split_authority("example.com").is_err());
        assert!(split_authority("example.com:http").is_err());
    }

    fn rewritten(head: &str) -> String {
        let request = parse_request(head).unwrap();
        let RequestKind::Forward { origin_form } = &request.kind else {
            panic!("expected a forward request");
        };
        rewrite_head(&request, origin_form)
    }

    #[test]
    fn a_missing_host_header_is_reconstructed() {
        let out = rewritten("GET http://example.com/a HTTP/1.1\r\n\r\n");
        assert!(out.contains("Host: example.com\r\n"), "{out}");
        assert!(out.starts_with("GET /a HTTP/1.1\r\n"), "{out}");
    }

    /// The allowlist approved a *host*. A `Host` header naming a different one
    /// would reach a different virtual host on the same server than the one
    /// that was checked.
    #[test]
    fn a_mismatched_host_header_does_not_survive() {
        let out = rewritten(
            "GET http://example.com/a HTTP/1.1\r\nHost: internal.example\r\nX-Keep: yes\r\n\r\n",
        );
        assert!(!out.contains("internal.example"), "{out}");
        assert!(out.contains("Host: example.com\r\n"), "{out}");
        assert!(
            out.contains("X-Keep: yes"),
            "ordinary headers survive: {out}"
        );
    }

    #[test]
    fn a_non_default_port_stays_in_the_host_header() {
        let out = rewritten("GET http://example.com:8080/a HTTP/1.1\r\n\r\n");
        assert!(out.contains("Host: example.com:8080\r\n"), "{out}");
    }
}
