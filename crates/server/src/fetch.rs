//! Fetching a URL a user gave (calendar subscriptions). HTTP/1.0 over TLS (or
//! plain HTTP), following a few redirects, with a size limit and timeouts.
//!
//! The server only connects to public addresses: a URL whose host resolves to
//! a loopback, private, link-local or otherwise internal address is refused,
//! so a user can't make the server reach into the local network.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REDIRECTS: usize = 3;

/// Whether an address is one the server may connect to on a user's behalf.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 shared address space
                || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18.0.0/15 benchmarking
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
        }
    }
}

struct Url {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Url> {
    let url = url.trim();
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("webcal://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        bail!("only http, https and webcal addresses are supported");
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        bail!("addresses with a user name are not supported");
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(']') || h.ends_with(']') => (h, p.parse::<u16>().map_err(|_| anyhow!("invalid port"))?),
        _ => (authority, if https { 443 } else { 80 }),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if host.is_empty() || path.chars().any(|c| c.is_control() || c == ' ') {
        bail!("invalid address");
    }
    Ok(Url { https, host, port, path: path.split('#').next().unwrap_or("/").to_string() })
}

async fn read_all<S: AsyncRead + Unpin>(s: &mut S, max: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 16384];
    loop {
        let n = tokio::time::timeout(TIMEOUT, s.read(&mut tmp)).await.map_err(|_| anyhow!("read timeout"))??;
        if n == 0 {
            return Ok(buf);
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > max {
            bail!("the response is too large");
        }
    }
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, u: &Url, max: usize) -> Result<Vec<u8>> {
    let req = format!("GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: Ondura\r\nAccept: text/calendar, */*\r\nConnection: close\r\n\r\n", u.path, u.host);
    s.write_all(req.as_bytes()).await?;
    read_all(s, max).await
}

/// GET a URL and return the body, refusing internal addresses.
pub async fn get(url: &str, max_bytes: usize) -> Result<String> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let u = parse_url(&url)?;
        let addrs: Vec<SocketAddr> = tokio::time::timeout(TIMEOUT, tokio::net::lookup_host((u.host.as_str(), u.port))).await.map_err(|_| anyhow!("lookup timeout"))??.collect();
        let addr = addrs.iter().copied().find(|a| is_public(a.ip())).ok_or_else(|| anyhow!("the address is not reachable from the internet"))?;
        if addrs.iter().any(|a| !is_public(a.ip())) {
            bail!("the address is not reachable from the internet");
        }
        let tcp = tokio::time::timeout(TIMEOUT, TcpStream::connect(addr)).await.map_err(|_| anyhow!("connect timeout"))??;
        let raw = if u.https {
            let name = rustls::pki_types::ServerName::try_from(u.host.clone())?;
            let mut tls = tokio::time::timeout(TIMEOUT, TlsConnector::from(crate::tls::client_config(true)).connect(name, tcp)).await.map_err(|_| anyhow!("TLS timeout"))??;
            exchange(&mut tls, &u, max_bytes).await?
        } else {
            let mut tcp = tcp;
            exchange(&mut tcp, &u, max_bytes).await?
        };
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow!("no response body"))?;
        let head = String::from_utf8_lossy(&raw[..split]).to_string();
        let body = &raw[split + 4..];
        let status: u16 = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
        if (300..400).contains(&status) {
            let loc = head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("location")).map(|(_, v)| v.trim().to_string()));
            let Some(loc) = loc else { bail!("redirect without a location") };
            url = if loc.starts_with('/') { format!("{}://{}:{}{}", if u.https { "https" } else { "http" }, u.host, u.port, loc) } else { loc };
            continue;
        }
        if status != 200 {
            bail!("the server answered with status {status}");
        }
        return Ok(String::from_utf8_lossy(body).into_owned());
    }
    bail!("too many redirects")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.0.1", "169.254.1.1", "100.64.1.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "88.198.1.1", "2a01:4f8::1"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn urls() {
        let u = parse_url("webcal://calendar.example.org/feed.ics#x").unwrap();
        assert!(u.https && u.port == 443 && u.host == "calendar.example.org" && u.path == "/feed.ics");
        let u = parse_url("http://example.org:8080").unwrap();
        assert!(!u.https && u.port == 8080 && u.path == "/");
        assert!(parse_url("ftp://x").is_err());
        assert!(parse_url("https://user@example.org/").is_err());
    }

    #[tokio::test]
    async fn refuses_internal_hosts() {
        assert!(get("http://127.0.0.1:1/x.ics", 1000).await.is_err());
        assert!(get("http://localhost/x.ics", 1000).await.unwrap_err().to_string().contains("not reachable"));
    }
}
