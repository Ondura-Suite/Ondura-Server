//! MTA-STS (RFC 8461) policy discovery and caching for outbound delivery.
//!
//! When a recipient domain publishes an `enforce` policy, delivery must use a
//! TLS connection whose certificate is valid for an MX host the policy allows;
//! otherwise the message is deferred rather than sent in cleartext. Policies are
//! fetched over HTTPS (with WebPKI verification) and cached for their `max_age`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::debug;

use crate::tls;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Enforce,
    Testing,
    None,
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub mode: Mode,
    /// Allowed MX host patterns, each possibly a `*.` wildcard.
    pub mx: Vec<String>,
}

impl Policy {
    /// Whether an MX host is allowed by the policy's `mx` patterns.
    pub fn allows_mx(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.mx.iter().any(|p| mx_match(p, &host))
    }
}

/// RFC 8461 MX matching: a `*.` wildcard matches exactly one leading label.
fn mx_match(pattern: &str, host: &str) -> bool {
    let p = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    match p.strip_prefix("*.") {
        Some(suffix) => host.split_once('.').map(|(_, rest)| rest == suffix).unwrap_or(false),
        None => host == p,
    }
}

struct Cached {
    policy: Option<Policy>,
    expires: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, Cached>> {
    static C: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The MTA-STS policy for a domain, cached by its `max_age`. `None` when the
/// domain publishes no usable policy.
pub async fn policy(domain: &str) -> Option<Policy> {
    let key = domain.to_ascii_lowercase();
    {
        let c = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = c.get(&key)
            && entry.expires > Instant::now()
        {
            return entry.policy.clone();
        }
    }
    let (policy, ttl) = fetch(&key).await;
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(key, Cached { policy: policy.clone(), expires: Instant::now() + ttl });
    policy
}

async fn fetch(domain: &str) -> (Option<Policy>, Duration) {
    // Remember a missing policy for an hour so it isn't re-fetched every message.
    let neg = Duration::from_secs(3600);
    let host = format!("mta-sts.{domain}");
    let body = match fetch_https(&host, "/.well-known/mta-sts.txt").await {
        Ok(b) => b,
        Err(e) => {
            debug!(%domain, "MTA-STS fetch failed: {e}");
            return (None, neg);
        }
    };
    parse(&body).map(|(p, max_age)| (Some(p), Duration::from_secs(max_age.clamp(3600, 604_800)))).unwrap_or((None, neg))
}

/// Parse a policy file into (policy, max_age). `None` if it is not an STSv1 file.
fn parse(body: &str) -> Option<(Policy, u64)> {
    let mut version_ok = false;
    let mut mode = Mode::None;
    let mut mx = Vec::new();
    let mut max_age: u64 = 86400;
    for line in body.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "version" => version_ok = v.eq_ignore_ascii_case("STSv1"),
            "mode" => {
                mode = match v.to_ascii_lowercase().as_str() {
                    "enforce" => Mode::Enforce,
                    "testing" => Mode::Testing,
                    _ => Mode::None,
                }
            }
            "mx" => mx.push(v.to_string()),
            "max_age" => {
                if let Ok(n) = v.parse() {
                    max_age = n;
                }
            }
            _ => {}
        }
    }
    version_ok.then_some((Policy { mode, mx }, max_age))
}

/// Minimal HTTPS GET with verified TLS and no redirect following (MTA-STS
/// forbids redirects). HTTP/1.0 so the body is simply everything after the
/// headers until the server closes the connection.
async fn fetch_https(host: &str, path: &str) -> anyhow::Result<String> {
    let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((host, 443)))
        .await
        .map_err(|_| anyhow::anyhow!("connect timeout"))??;
    let name = rustls::pki_types::ServerName::try_from(host.to_string())?;
    let mut tls = tokio::time::timeout(Duration::from_secs(10), TlsConnector::from(tls::client_config(true)).connect(name, tcp))
        .await
        .map_err(|_| anyhow::anyhow!("TLS timeout"))??;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: Ondura\r\nAccept: text/plain\r\n\r\n");
    tls.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), tls.read(&mut tmp)).await.map_err(|_| anyhow::anyhow!("read timeout"))??;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 65_536 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no response body"))?;
    let status = head.lines().next().unwrap_or("");
    if !status.contains(" 200") {
        anyhow::bail!("HTTP status: {status}");
    }
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_enforce_policy() {
        let body = "version: STSv1\nmode: enforce\nmx: mail.example.com\nmx: *.example.net\nmax_age: 604800\n";
        let (p, age) = parse(body).unwrap();
        assert_eq!(p.mode, Mode::Enforce);
        assert_eq!(age, 604800);
        assert!(p.allows_mx("mail.example.com"));
        assert!(p.allows_mx("mx1.example.net")); // wildcard, one label
        assert!(!p.allows_mx("a.b.example.net")); // wildcard is single-label
        assert!(!p.allows_mx("evil.test"));
    }

    #[test]
    fn rejects_non_stsv1() {
        assert!(parse("mode: enforce\nmx: x\n").is_none());
    }
}
