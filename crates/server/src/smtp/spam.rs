//! Optional spam check with rspamd (`/checkv2` over plain HTTP, normally on
//! localhost).

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::Spam;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    Clean(f64),
    Junk(f64),
    Reject,
}

pub async fn check(url: &str, raw: &[u8], ip: IpAddr, helo: &str, from: &str, rcpts: &[String], cfg: &Spam) -> Result<Verdict> {
    let rest = url.strip_prefix("http://").ok_or_else(|| anyhow!("rspamd URL must start with http://"))?;
    let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, String::new()));
    let path = format!("{}/checkv2", path.trim_end_matches('/'));
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\nIP: {ip}\r\nHelo: {helo}\r\nFrom: {from}\r\n",
        raw.len()
    );
    for r in rcpts {
        head.push_str(&format!("Rcpt: {r}\r\n"));
    }
    head.push_str("\r\n");
    let fut = async {
        let mut s = TcpStream::connect(host).await.with_context(|| format!("connect {host}"))?;
        s.write_all(head.as_bytes()).await?;
        s.write_all(raw).await?;
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).await?;
        Ok::<_, anyhow::Error>(resp)
    };
    let resp = tokio::time::timeout(Duration::from_secs(30), fut).await.map_err(|_| anyhow!("rspamd timeout"))??;
    let text = String::from_utf8_lossy(&resp);
    let (status, body) = text.split_once("\r\n\r\n").ok_or_else(|| anyhow!("invalid rspamd response"))?;
    if !status.starts_with("HTTP/1.1 200") && !status.starts_with("HTTP/1.0 200") {
        bail!("rspamd: {}", status.lines().next().unwrap_or_default());
    }
    // Chunked bodies: take the JSON object between the first '{' and last '}'.
    let json = &body[body.find('{').unwrap_or(0)..body.rfind('}').map(|i| i + 1).unwrap_or(body.len())];
    let v: serde_json::Value = serde_json::from_str(json).context("rspamd JSON")?;
    let score = v["score"].as_f64().unwrap_or(0.0);
    let action = v["action"].as_str().unwrap_or("no action");
    Ok(verdict(action, score, cfg))
}

fn verdict(action: &str, score: f64, cfg: &Spam) -> Verdict {
    if cfg.reject_score.is_some_and(|r| score >= r) || (cfg.reject_score.is_none() && action == "reject") {
        return Verdict::Reject;
    }
    if cfg.junk_score.is_some_and(|j| score >= j) || (cfg.junk_score.is_none() && matches!(action, "add header" | "rewrite subject" | "soft reject")) {
        return Verdict::Junk(score);
    }
    Verdict::Clean(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts() {
        let cfg = Spam::default();
        assert_eq!(verdict("reject", 20.0, &cfg), Verdict::Reject);
        assert_eq!(verdict("add header", 7.0, &cfg), Verdict::Junk(7.0));
        assert_eq!(verdict("no action", 1.0, &cfg), Verdict::Clean(1.0));
        let cfg = Spam { rspamd: None, junk_score: Some(5.0), reject_score: Some(15.0) };
        assert_eq!(verdict("no action", 6.0, &cfg), Verdict::Junk(6.0));
    }
}
