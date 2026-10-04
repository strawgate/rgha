//! Transparent egress proxy enforcing per-VM domain allowlists for the
//! self-hosted backends.
//!
//! The host redirects a locked-down VM's TCP 443 and 80 to this proxy. It
//! reads the TLS ClientHello SNI (443) or the HTTP `Host` header (80),
//! checks it against the allowlist registered for the VM's source address,
//! and only then connects upstream — by *name*, so a client can't pair an
//! allowed SNI with an arbitrary destination IP. Upstream addresses that
//! resolve to private, loopback or link-local ranges are refused (DNS
//! rebinding). Connections without an SNI (e.g. ECH) and from unknown
//! sources are dropped. Everything else from the VM is dropped by the
//! firewall, except DNS to the configured resolver.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const TLS_PORT: u16 = 15443;
pub const HTTP_PORT: u16 = 15080;

/// Allowlists keyed by the VM's (unique) source address.
#[derive(Clone, Default)]
pub struct EgressProxy {
    allow: Arc<RwLock<HashMap<Ipv4Addr, Arc<Vec<String>>>>>,
}

impl EgressProxy {
    pub fn register(&self, src: Ipv4Addr, domains: Vec<String>) {
        self.allow.write().expect("allow lock").insert(src, Arc::new(domains));
    }

    pub fn unregister(&self, src: Ipv4Addr) {
        self.allow.write().expect("allow lock").remove(&src);
    }

    fn allowlist(&self, src: IpAddr) -> Option<Arc<Vec<String>>> {
        match src {
            IpAddr::V4(v4) => self.allow.read().expect("allow lock").get(&v4).cloned(),
            IpAddr::V6(_) => None,
        }
    }

    /// Starts the TLS and HTTP listeners (on all addresses; connections from
    /// sources without a registered allowlist are closed immediately).
    pub async fn start(&self) -> anyhow::Result<()> {
        for (port, kind) in [(TLS_PORT, Kind::Tls), (HTTP_PORT, Kind::Http)] {
            let listener =
                TcpListener::bind(("0.0.0.0", port)).await.with_context(|| format!("binding egress proxy :{port}"))?;
            let this = self.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((sock, peer)) = listener.accept().await else { continue };
                    let this = this.clone();
                    tokio::spawn(async move {
                        if let Err(e) = this.handle(sock, peer, kind).await {
                            tracing::debug!(%peer, error = %format!("{e:#}"), "egress connection refused");
                        }
                    });
                }
            });
        }
        tracing::info!(tls = TLS_PORT, http = HTTP_PORT, "egress proxy listening");
        Ok(())
    }

    async fn handle(&self, mut client: TcpStream, peer: SocketAddr, kind: Kind) -> anyhow::Result<()> {
        let allow = self.allowlist(peer.ip()).context("source has no allowlist")?;
        let mut buf = vec![0u8; 16 * 1024];
        let mut n = 0;
        let host = loop {
            let read = tokio::time::timeout(Duration::from_secs(10), client.read(&mut buf[n..])).await??;
            if read == 0 {
                bail!("client closed before sending a hostname");
            }
            n += read;
            let parsed = match kind {
                Kind::Tls => parse_sni(&buf[..n]),
                Kind::Http => parse_http_host(&buf[..n]),
            };
            match parsed {
                Parse::Host(h) => break h,
                Parse::NeedMore if n < buf.len() => continue,
                Parse::NeedMore => bail!("no hostname in the first {} bytes", buf.len()),
                Parse::Invalid => bail!("no SNI/Host (not TLS/HTTP, or encrypted ClientHello)"),
            }
        };
        if !allow.iter().any(|p| domain_match(p, &host)) {
            tracing::info!(%peer, %host, "egress blocked by allowlist");
            bail!("{host} is not allowlisted");
        }
        let port = match kind {
            Kind::Tls => 443,
            Kind::Http => 80,
        };
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port)).await?.collect();
        let addr = addrs
            .into_iter()
            .find(|a| is_public(a.ip()))
            .with_context(|| format!("{host} resolves only to non-public addresses"))?;
        let mut upstream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr)).await??;
        upstream.write_all(&buf[..n]).await?;
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Tls,
    Http,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Parse {
    Host(String),
    NeedMore,
    Invalid,
}

/// Case-insensitive domain match: `*.example.com` matches any subdomain
/// (not the apex); anything else must match exactly.
pub fn domain_match(pattern: &str, host: &str) -> bool {
    let (p, h) = (pattern.to_ascii_lowercase(), host.trim_end_matches('.').to_ascii_lowercase());
    match p.strip_prefix("*.") {
        Some(suffix) => h.len() > suffix.len() + 1 && h.ends_with(&format!(".{suffix}")),
        None => p == h,
    }
}

pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT
                || o[0] == 0)
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// Extracts the SNI host name from a TLS ClientHello.
pub(crate) fn parse_sni(b: &[u8]) -> Parse {
    // TLS record header: type(1)=22 handshake, version(2), length(2).
    if b.is_empty() {
        return Parse::NeedMore;
    }
    if b[0] != 22 {
        return Parse::Invalid;
    }
    if b.len() < 5 {
        return Parse::NeedMore;
    }
    let rec_len = u16::from_be_bytes([b[3], b[4]]) as usize;
    if b.len() < 5 + rec_len {
        return Parse::NeedMore;
    }
    let mut p = Cursor { b: &b[5..5 + rec_len], i: 0 };
    let res = (|| -> Option<Option<String>> {
        if p.u8()? != 1 {
            return None; // not a ClientHello
        }
        let _len = p.u24()?;
        p.skip(2 + 32)?; // version, random
        let sid = p.u8()? as usize;
        p.skip(sid)?;
        let cs = p.u16()? as usize;
        p.skip(cs)?;
        let comp = p.u8()? as usize;
        p.skip(comp)?;
        if p.remaining() == 0 {
            return Some(None);
        }
        let ext_total = p.u16()? as usize;
        let end = p.i + ext_total;
        while p.i + 4 <= end {
            let ty = p.u16()?;
            let len = p.u16()? as usize;
            if ty == 0 {
                // server_name: list len(2), [type(1)=0, len(2), name]
                let _list = p.u16()?;
                let name_type = p.u8()?;
                let name_len = p.u16()? as usize;
                let name = p.take(name_len)?;
                if name_type != 0 {
                    return Some(None);
                }
                return Some(std::str::from_utf8(name).ok().map(str::to_string));
            }
            p.skip(len)?;
        }
        Some(None)
    })();
    match res {
        Some(Some(h)) if !h.is_empty() => Parse::Host(h),
        _ => Parse::Invalid,
    }
}

/// Extracts the Host header from the start of an HTTP/1.x request.
pub(crate) fn parse_http_host(b: &[u8]) -> Parse {
    let Some(end) = b.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if b.len() < 8192 { Parse::NeedMore } else { Parse::Invalid };
    };
    let Ok(head) = std::str::from_utf8(&b[..end]) else { return Parse::Invalid };
    for line in head.split("\r\n").skip(1) {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("host")
        {
            let host = v.trim();
            let host = host
                .rsplit_once(':')
                .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
                .map(|(h, _)| h)
                .unwrap_or(host);
            return if host.is_empty() { Parse::Invalid } else { Parse::Host(host.to_string()) };
        }
    }
    Parse::Invalid
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.i)
    }
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.b.get(self.i..self.i + n)?;
        self.i += n;
        Some(s)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_be_bytes([s[0], s[1]]))
    }
    fn u24(&mut self) -> Option<u32> {
        self.take(3).map(|s| u32::from_be_bytes([0, s[0], s[1], s[2]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;

    /// A real ClientHello produced by rustls for `name`.
    fn client_hello(name: &str) -> Vec<u8> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(std::iter::empty::<rustls::pki_types::CertificateDer>());
        let cfg = rustls::ClientConfig::builder_with_provider(StdArc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server = rustls::pki_types::ServerName::try_from(name.to_string()).unwrap();
        let mut conn = rustls::ClientConnection::new(StdArc::new(cfg), server).unwrap();
        let mut out = Vec::new();
        conn.write_tls(&mut out).unwrap();
        out
    }

    #[test]
    fn sni_from_real_client_hello() {
        let hello = client_hello("api.github.com");
        assert_eq!(parse_sni(&hello), Parse::Host("api.github.com".into()));
        assert_eq!(parse_sni(&hello[..10]), Parse::NeedMore, "partial record");
        assert_eq!(parse_sni(b"GET / HTTP/1.1\r\n"), Parse::Invalid, "not TLS");
    }

    #[test]
    fn http_host_header() {
        assert_eq!(
            parse_http_host(b"GET / HTTP/1.1\r\nHost: Example.com:80\r\nX: y\r\n\r\n"),
            Parse::Host("Example.com".into())
        );
        assert_eq!(parse_http_host(b"GET / HTTP/1.1\r\nHost: a.b"), Parse::NeedMore);
        assert_eq!(parse_http_host(b"GET / HTTP/1.1\r\nX: y\r\n\r\n"), Parse::Invalid);
    }

    #[test]
    fn domain_matching() {
        assert!(domain_match("github.com", "GitHub.com."));
        assert!(!domain_match("github.com", "api.github.com"));
        assert!(domain_match("*.actions.githubusercontent.com", "pipelines.actions.githubusercontent.com"));
        assert!(
            !domain_match("*.actions.githubusercontent.com", "actions.githubusercontent.com"),
            "wildcard excludes apex"
        );
        assert!(!domain_match("*.github.com", "evilgithub.com"));
        assert!(!domain_match("*.github.com", "github.com.evil.io"));
    }

    #[test]
    fn non_public_upstreams_refused() {
        for ip in ["10.1.2.3", "172.17.0.1", "192.168.1.1", "127.0.0.1", "169.254.169.254", "100.64.0.1", "0.0.0.0"] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        assert!(is_public("140.82.112.3".parse().unwrap()));
    }

    #[tokio::test]
    async fn proxy_blocks_unregistered_and_unlisted() {
        let p = EgressProxy::default();
        let src: Ipv4Addr = "127.0.0.1".parse().unwrap();
        // Unregistered source: no allowlist.
        assert!(p.allowlist(IpAddr::V4(src)).is_none());
        p.register(src, vec!["*.github.com".into()]);
        let allow = p.allowlist(IpAddr::V4(src)).unwrap();
        assert!(allow.iter().any(|d| domain_match(d, "api.github.com")));
        assert!(!allow.iter().any(|d| domain_match(d, "example.com")));
        p.unregister(src);
        assert!(p.allowlist(IpAddr::V4(src)).is_none());
    }
}
