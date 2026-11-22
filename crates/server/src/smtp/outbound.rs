//! Outbound queue: delivery to remote servers via MX (or a configured
//! relay), retries with back-off, bounces.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use tokio::net::TcpStream;
use tracing::{info, warn};

use crate::app::Shared;
use crate::mime::{self, Outgoing};
use crate::model::Person;
use crate::store::{QueuedMessage, now};
use crate::tls;

use super::conn::{Conn, Stream};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    Permanent(String),
    Temporary(String),
}

/// Queue worker: runs forever.
pub async fn run(app: Shared) {
    loop {
        match app.store.due_messages(20) {
            Ok(list) => {
                for msg in list {
                    let app = app.clone();
                    // One message at a time per worker keeps the load predictable.
                    attempt(&app, msg).await;
                }
            }
            Err(e) => warn!("queue: {e:#}"),
        }
        tokio::select! {
            _ = app.queue_wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
    }
}

fn backoff(attempts: i64) -> i64 {
    const STEPS: [i64; 7] = [300, 600, 1800, 3600, 7200, 14_400, 28_800];
    STEPS[(attempts as usize).min(STEPS.len() - 1)]
}

async fn attempt(app: &Shared, msg: QueuedMessage) {
    let mut by_domain: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in &msg.recipients {
        let d = r.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()).unwrap_or_default();
        by_domain.entry(d).or_default().push(r.clone());
    }
    let mut retry = Vec::new();
    let mut failed = Vec::new();
    let mut last_error = String::new();
    for (domain, rcpts) in by_domain {
        let results = deliver_domain(app, &domain, &msg.sender, &rcpts, &msg.mime).await;
        for (rcpt, outcome) in results {
            match outcome {
                Outcome::Delivered => info!(id = msg.id, %rcpt, "delivered"),
                Outcome::Permanent(e) => {
                    warn!(id = msg.id, %rcpt, "permanent failure: {e}");
                    failed.push((rcpt, e));
                }
                Outcome::Temporary(e) => {
                    last_error = format!("{rcpt}: {e}");
                    retry.push((rcpt, e));
                }
            }
        }
    }
    let expired = now() - msg.created > app.cfg.smtp.queue_lifetime_days * 86_400;
    if expired {
        failed.extend(retry.drain(..).map(|(r, e)| (r, format!("{e} (gave up after {} days)", app.cfg.smtp.queue_lifetime_days))));
    }
    if !failed.is_empty() {
        bounce(app, &msg.sender, &msg.mime, &failed).await;
    }
    let result = if retry.is_empty() {
        app.store.dequeue(msg.id)
    } else {
        let rcpts: Vec<String> = retry.into_iter().map(|(r, _)| r).collect();
        app.store.defer(msg.id, &rcpts, now() + backoff(msg.attempts), &last_error)
    };
    if let Err(e) = result {
        warn!("queue update failed: {e:#}");
    }
}

/// MX hosts of a domain, best first. `Err` for a null MX.
async fn mx_hosts(app: &Shared, domain: &str) -> std::result::Result<Vec<String>, Outcome> {
    let Some(dns) = &app.dns else { return Ok(vec![domain.to_string()]) };
    match dns.0.mx_lookup(format!("{domain}.")).await {
        Ok(lookup) => {
            let mut mx: Vec<(u16, String)> = lookup.answers().iter().filter_map(|r| match &r.data { hickory_resolver::proto::rr::RData::MX(m) => Some((m.preference, m.exchange.to_utf8())), _ => None }).collect();
            mx.sort();
            if mx.len() == 1 && (mx[0].1 == "." || mx[0].1.is_empty()) {
                return Err(Outcome::Permanent("556 5.1.10 Domain does not accept mail (null MX)".into()));
            }
            if mx.is_empty() {
                return Ok(vec![domain.to_string()]);
            }
            Ok(mx.into_iter().map(|(_, h)| h.trim_end_matches('.').to_string()).collect())
        }
        Err(e) if e.is_no_records_found() => Ok(vec![domain.to_string()]),
        Err(e) if e.is_nx_domain() => Err(Outcome::Permanent(format!("550 5.1.2 Domain {domain} does not exist"))),
        Err(e) => Err(Outcome::Temporary(format!("DNS lookup failed: {e}"))),
    }
}

async fn deliver_domain(app: &Shared, domain: &str, sender: &str, rcpts: &[String], raw: &[u8]) -> Vec<(String, Outcome)> {
    let all = |o: Outcome| rcpts.iter().map(|r| (r.clone(), o.clone())).collect::<Vec<_>>();
    if let Some(relay) = &app.cfg.smtp.relay {
        let (host, port) = relay.rsplit_once(':').map(|(h, p)| (h.to_string(), p.parse().unwrap_or(587))).unwrap_or((relay.clone(), 587));
        return match transmit(app, &host, port, true, sender, rcpts, raw).await {
            Ok(r) => r,
            Err(e) => all(Outcome::Temporary(format!("relay {host}: {e:#}"))),
        };
    }
    let hosts = match mx_hosts(app, domain).await {
        Ok(h) => h,
        Err(o) => return all(o),
    };
    let mut last = Outcome::Temporary("no mail server reachable".into());
    for host in hosts.iter().take(5) {
        match transmit(app, host, 25, false, sender, rcpts, raw).await {
            Ok(results) => return results,
            Err(e) => {
                warn!(%host, "SMTP delivery attempt failed: {e:#}");
                last = Outcome::Temporary(format!("{host}: {e:#}"));
            }
        }
    }
    all(last)
}

fn expect(code: u16, lines: &[String], ok: u16) -> Result<()> {
    if code / 100 == ok / 100 {
        Ok(())
    } else {
        Err(anyhow!("{code} {}", lines.join(" ")))
    }
}

/// One SMTP transaction. `Err` means the whole attempt failed temporarily
/// (connection problems); per-recipient results otherwise.
async fn transmit(app: &Shared, host: &str, port: u16, relay: bool, sender: &str, rcpts: &[String], raw: &[u8]) -> Result<Vec<(String, Outcome)>> {
    let tcp = tokio::time::timeout(Duration::from_secs(30), TcpStream::connect((host, port))).await.map_err(|_| anyhow!("connect timeout"))??;
    let mut c = Conn::new(Stream::Plain(tcp));
    c.timeout = Duration::from_secs(300);
    if relay && port == 465 {
        c.connect_tls(tls::client_config(true), host).await?;
    }
    let (code, lines) = c.read_reply().await?;
    if code / 100 != 2 {
        return Ok(rcpts.iter().map(|r| (r.clone(), classify(code, &lines))).collect());
    }
    c.reply(&format!("EHLO {}", app.hostname())).await?;
    let (code, mut ext) = c.read_reply().await?;
    expect(code, &ext, 250)?;
    let has = |ext: &[String], name: &str| ext.iter().any(|l| l.split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case(name)));
    if !c.is_tls() && has(&ext, "STARTTLS") {
        c.reply("STARTTLS").await?;
        let (code, lines) = c.read_reply().await?;
        if code == 220 {
            c.connect_tls(tls::client_config(relay), host).await?;
            c.reply(&format!("EHLO {}", app.hostname())).await?;
            let (code, e) = c.read_reply().await?;
            expect(code, &e, 250)?;
            ext = e;
        } else if relay {
            return Err(anyhow!("relay refused STARTTLS: {code} {}", lines.join(" ")));
        }
    } else if relay && !c.is_tls() {
        return Err(anyhow!("relay does not offer STARTTLS"));
    }
    if relay && let (Some(u), Some(p)) = (&app.cfg.smtp.relay_username, &app.cfg.smtp.relay_password) {
        c.reply(&format!("AUTH PLAIN {}", B64.encode(format!("\0{u}\0{p}")))).await?;
        let (code, lines) = c.read_reply().await?;
        expect(code, &lines, 235)?;
    }
    let size = if has(&ext, "SIZE") { format!(" SIZE={}", raw.len()) } else { String::new() };
    c.reply(&format!("MAIL FROM:<{sender}>{size}")).await?;
    let (code, lines) = c.read_reply().await?;
    if code / 100 != 2 {
        let o = classify(code, &lines);
        let _ = c.reply("QUIT").await;
        return Ok(rcpts.iter().map(|r| (r.clone(), o.clone())).collect());
    }
    let mut results = Vec::new();
    let mut accepted = Vec::new();
    for r in rcpts {
        c.reply(&format!("RCPT TO:<{r}>")).await?;
        let (code, lines) = c.read_reply().await?;
        if code / 100 == 2 {
            accepted.push(r.clone());
        } else {
            results.push((r.clone(), classify(code, &lines)));
        }
    }
    if accepted.is_empty() {
        let _ = c.reply("QUIT").await;
        return Ok(results);
    }
    c.reply("DATA").await?;
    let (code, lines) = c.read_reply().await?;
    if code != 354 {
        let o = classify(code, &lines);
        results.extend(accepted.into_iter().map(|r| (r, o.clone())));
        return Ok(results);
    }
    c.write(&dot_stuff(raw)).await?;
    let (code, lines) = c.read_reply().await?;
    let o = if code / 100 == 2 { Outcome::Delivered } else { classify(code, &lines) };
    results.extend(accepted.into_iter().map(|r| (r, o.clone())));
    let _ = c.reply("QUIT").await;
    Ok(results)
}

fn classify(code: u16, lines: &[String]) -> Outcome {
    let text = format!("{code} {}", lines.join(" "));
    if code / 100 == 5 { Outcome::Permanent(text) } else { Outcome::Temporary(text) }
}

fn dot_stuff(raw: &[u8]) -> Vec<u8> {
    let raw = mime::crlf(raw);
    let mut out = Vec::with_capacity(raw.len() + 64);
    let mut line_start = true;
    for &b in &raw {
        if line_start && b == b'.' {
            out.push(b'.');
        }
        out.push(b);
        line_start = b == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

/// Non-delivery report to the sender.
pub async fn bounce(app: &Shared, sender: &str, original: &[u8], failures: &[(String, String)]) {
    if sender.is_empty() {
        return;
    }
    let headers = &original[..mime::find_header_end(original)];
    let meta = mime::parse_meta(original);
    let list: String = failures.iter().map(|(r, e)| format!("  {r}\n    {e}\n")).collect();
    let text = format!(
        "Your message could not be delivered to one or more recipients.\n\nSubject: {}\n\n{list}\nThis is an automatically generated message from {}.\n",
        meta.subject,
        app.hostname()
    );
    let out = Outgoing {
        from: Person::new("Mail Delivery System", format!("mailer-daemon@{}", app.hostname())),
        to: vec![Person::new("", sender)],
        subject: format!("Undeliverable: {}", meta.subject),
        text,
        attachment: Some(("text/rfc822-headers".into(), "headers.txt".into(), headers.to_vec())),
        headers: vec![("Auto-Submitted".into(), "auto-replied".into())],
        ..Default::default()
    };
    let raw = out.build(app.hostname());
    let domain = sender.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
    let result = if app.store.is_local_domain(domain) {
        match app.store.resolve_address(sender) {
            Ok(Some((uid, _))) => (super::delivery::deliver(app, uid, &raw, super::delivery::Options { junk: false, envelope_from: String::new() })).await,
            _ => Ok(()),
        }
    } else {
        app.store.enqueue("", &[sender.to_string()], &raw).map(|_| app.queue_wake.notify_one())
    };
    if let Err(e) = result {
        warn!("bounce to {sender} failed: {e:#}");
    }
}

pub fn spawn(app: Shared) {
    let _ = Arc::strong_count(&app);
    tokio::spawn(run(app));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stuffing() {
        assert_eq!(dot_stuff(b"a\n.b\r\n..c"), b"a\r\n..b\r\n...c\r\n.\r\n".to_vec());
        assert_eq!(backoff(0), 300);
        assert_eq!(backoff(99), 28_800);
    }

    #[tokio::test]
    async fn bounces_to_local_sender() {
        let (app, user) = crate::app::App::test();
        bounce(&app, &user.email, b"Subject: Hallo\r\n\r\nx", &[("x@nowhere.invalid".into(), "550 no".into())]).await;
        let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
        let items = app.store.items(inbox.id, 0, 10).unwrap();
        assert_eq!(items[0].subject, "Undeliverable: Hallo");
    }
}
