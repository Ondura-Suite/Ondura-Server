//! SMTP: receiving mail from other servers (port 25), submission by mail
//! clients (587 with STARTTLS, 465 with implicit TLS) and the outbound queue.

pub mod conn;
pub mod delivery;
pub mod dkim;
pub mod mtasts;
pub mod outbound;
pub mod spam;
pub mod submit;

use std::net::{IpAddr, SocketAddr};

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::app::Shared;
use crate::auth;
use crate::mime;
use crate::store::User;
use conn::{Conn, DataError, Stream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Port 25: mail for local mailboxes from other servers.
    Inbound,
    /// Port 587: authenticated submission, STARTTLS.
    Submission,
    /// Port 465: authenticated submission, implicit TLS.
    Submissions,
}

/// Maximum simultaneous SMTP connections across all clients.
const MAX_CONNECTIONS: usize = 512;
/// Maximum simultaneous SMTP connections from a single remote IP.
const MAX_CONNECTIONS_PER_IP: usize = 20;

pub async fn serve(app: Shared, addr: SocketAddr, mode: Mode) -> Result<()> {
    let listener = TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!("bind SMTP {addr}: {e}"))?;
    info!(%addr, ?mode, "SMTP listening");
    let limiter = crate::netlimit::ConnLimiter::new(MAX_CONNECTIONS, MAX_CONNECTIONS_PER_IP);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("SMTP accept: {e}");
                continue;
            }
        };
        let Some(guard) = limiter.admit(peer.ip()) else {
            debug!(%peer, "SMTP connection refused: limit reached");
            // Drop without a banner; a well-behaved peer retries later.
            continue;
        };
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = session(app, tcp, peer, mode).await {
                debug!(%peer, "SMTP session ended: {e}");
            }
            drop(guard);
        });
    }
}

struct Rcpt {
    address: String,
    /// Local mailbox, if any.
    user_id: Option<i64>,
}

struct Session {
    app: Shared,
    peer: SocketAddr,
    mode: Mode,
    helo: Option<String>,
    user: Option<User>,
    mail_from: Option<String>,
    rcpts: Vec<Rcpt>,
    bad_commands: u32,
    auth_failures: u32,
}

const MAX_RCPTS: usize = 500;

pub async fn session(app: Shared, tcp: tokio::net::TcpStream, peer: SocketAddr, mode: Mode) -> std::io::Result<()> {
    let _ = tcp.set_nodelay(true);
    let mut conn = Conn::new(Stream::Plain(tcp));
    if mode == Mode::Submissions {
        let Some(tls) = app.smtp_tls.clone() else { return Ok(()) };
        conn.accept_tls(TlsAcceptor::from(tls)).await?;
    }
    let mut s = Session { app, peer, mode, helo: None, user: None, mail_from: None, rcpts: Vec::new(), bad_commands: 0, auth_failures: 0 };
    conn.reply(&format!("220 {} ESMTP Ondura Server ready", s.app.hostname())).await?;
    loop {
        let Some(line) = conn.read_line(4096).await? else { return Ok(()) };
        let (verb, arg) = match line.split_once(' ') {
            Some((v, a)) => (v.to_ascii_uppercase(), a.trim().to_string()),
            None => (line.trim().to_ascii_uppercase(), String::new()),
        };
        match verb.as_str() {
            "EHLO" | "HELO" | "LHLO" => {
                if arg.is_empty() {
                    conn.reply("501 5.5.4 Syntax: EHLO hostname").await?;
                    continue;
                }
                s.helo = Some(arg.clone());
                s.reset();
                if verb == "HELO" {
                    conn.reply(&format!("250 {}", s.app.hostname())).await?;
                    continue;
                }
                let mut ext = vec![
                    format!("{} Hello {arg} [{}]", s.app.hostname(), s.peer.ip()),
                    "PIPELINING".into(),
                    format!("SIZE {}", s.app.cfg.smtp.max_message_size),
                    "8BITMIME".into(),
                    "ENHANCEDSTATUSCODES".into(),
                    "SMTPUTF8".into(),
                ];
                if !conn.is_tls() && s.app.smtp_tls.is_some() {
                    ext.push("STARTTLS".into());
                }
                if s.mode != Mode::Inbound && (conn.is_tls() || s.peer.ip().is_loopback()) {
                    ext.push("AUTH PLAIN LOGIN".into());
                }
                let n = ext.len();
                let text: Vec<String> = ext.iter().enumerate().map(|(i, e)| format!("250{}{e}", if i + 1 == n { ' ' } else { '-' })).collect();
                conn.reply(&text.join("\n")).await?;
            }
            "STARTTLS" => {
                let Some(tls) = s.app.smtp_tls.clone().filter(|_| !conn.is_tls()) else {
                    conn.reply("502 5.5.1 STARTTLS not available").await?;
                    continue;
                };
                if conn.has_buffered() {
                    conn.reply("501 5.5.4 No commands allowed after STARTTLS").await?;
                    return Ok(());
                }
                conn.reply("220 2.0.0 Ready to start TLS").await?;
                conn.accept_tls(TlsAcceptor::from(tls)).await?;
                s.helo = None;
                s.reset();
            }
            "AUTH" => {
                if s.mode == Mode::Inbound {
                    conn.reply("503 5.5.1 Authentication is only available on the submission ports").await?;
                    continue;
                }
                if !conn.is_tls() && !s.peer.ip().is_loopback() {
                    conn.reply("538 5.7.11 Encryption required for requested authentication mechanism").await?;
                    continue;
                }
                if s.user.is_some() {
                    conn.reply("503 5.5.1 Already authenticated").await?;
                    continue;
                }
                match s.auth(&mut conn, &arg).await? {
                    Some(u) => {
                        info!(user = %u.email, peer = %s.peer, "SMTP login");
                        s.user = Some(u);
                        conn.reply("235 2.7.0 Authentication successful").await?;
                    }
                    None => {
                        s.auth_failures += 1;
                        conn.reply("535 5.7.8 Authentication credentials invalid").await?;
                        if s.auth_failures >= 3 {
                            return Ok(());
                        }
                    }
                }
            }
            "MAIL" => {
                if s.helo.is_none() {
                    conn.reply("503 5.5.1 Send EHLO first").await?;
                    continue;
                }
                if s.mode != Mode::Inbound && s.user.is_none() {
                    conn.reply("530 5.7.0 Authentication required").await?;
                    continue;
                }
                if s.mail_from.is_some() {
                    conn.reply("503 5.5.1 Sender already given").await?;
                    continue;
                }
                let Some((addr, params)) = parse_path(&arg, "FROM:") else {
                    conn.reply("501 5.5.4 Syntax: MAIL FROM:<address>").await?;
                    continue;
                };
                if let Some(size) = params.iter().find_map(|p| p.strip_prefix("SIZE=").or_else(|| p.strip_prefix("size="))).and_then(|v| v.parse::<usize>().ok())
                    && size > s.app.cfg.smtp.max_message_size
                {
                    conn.reply("552 5.3.4 Message too big").await?;
                    continue;
                }
                if let Some(u) = &s.user {
                    let allowed = s.app.store.send_as_addresses(u.id).unwrap_or_default();
                    if !addr.is_empty() && !allowed.iter().any(|a| a.eq_ignore_ascii_case(&addr)) {
                        conn.reply("553 5.7.1 Sender address not owned by user").await?;
                        continue;
                    }
                }
                s.mail_from = Some(addr);
                conn.reply("250 2.1.0 OK").await?;
            }
            "RCPT" => {
                if s.mail_from.is_none() {
                    conn.reply("503 5.5.1 Need MAIL first").await?;
                    continue;
                }
                let Some((addr, _)) = parse_path(&arg, "TO:") else {
                    conn.reply("501 5.5.4 Syntax: RCPT TO:<address>").await?;
                    continue;
                };
                if addr.is_empty() || !addr.contains('@') {
                    conn.reply("501 5.1.3 Invalid recipient address").await?;
                    continue;
                }
                if s.rcpts.len() >= MAX_RCPTS {
                    conn.reply("452 4.5.3 Too many recipients").await?;
                    continue;
                }
                let domain = addr.rsplit_once('@').map(|(_, d)| d.to_string()).unwrap_or_default();
                let local = s.app.store.is_local_domain(&domain);
                let user_id = if local { s.app.store.resolve_address(&addr).ok().flatten().map(|(id, _)| id) } else { None };
                match (s.mode, local, user_id) {
                    (_, true, None) => {
                        conn.reply("550 5.1.1 Mailbox unavailable").await?;
                        continue;
                    }
                    (Mode::Inbound, false, _) => {
                        conn.reply("554 5.7.1 Relay access denied").await?;
                        continue;
                    }
                    _ => {}
                }
                if let Some(id) = user_id {
                    match s.app.store.user(id).ok().flatten() {
                        Some(u) if !u.enabled => {
                            conn.reply("550 5.2.1 Mailbox disabled").await?;
                            continue;
                        }
                        // The account exists but has no mailbox, so there is
                        // nowhere to deliver.
                        Some(u) if !u.mailbox => {
                            conn.reply("550 5.1.1 Mailbox unavailable").await?;
                            continue;
                        }
                        _ => {}
                    }
                }
                s.rcpts.push(Rcpt { address: addr, user_id });
                conn.reply("250 2.1.5 OK").await?;
            }
            "DATA" => {
                if s.rcpts.is_empty() {
                    conn.reply("503 5.5.1 Need RCPT first").await?;
                    continue;
                }
                conn.reply("354 Start mail input; end with <CRLF>.<CRLF>").await?;
                match conn.read_data(s.app.cfg.smtp.max_message_size).await {
                    Ok(raw) => {
                        let tls = conn.is_tls();
                        let reply = s.accept_message(raw, tls).await;
                        conn.reply(&reply).await?;
                    }
                    Err(DataError::TooLarge) => conn.reply("552 5.3.4 Message too big").await?,
                    Err(DataError::Io(e)) => return Err(e),
                }
                s.reset();
            }
            "BDAT" => conn.reply("502 5.5.1 CHUNKING not supported").await?,
            "RSET" => {
                s.reset();
                conn.reply("250 2.0.0 OK").await?;
            }
            "NOOP" => conn.reply("250 2.0.0 OK").await?,
            "VRFY" | "EXPN" => conn.reply("252 2.5.0 Cannot VRFY user").await?,
            "HELP" => conn.reply("214 2.0.0 See RFC 5321").await?,
            "QUIT" => {
                conn.reply("221 2.0.0 Bye").await?;
                return Ok(());
            }
            _ => {
                s.bad_commands += 1;
                conn.reply("500 5.5.2 Command not recognized").await?;
                if s.bad_commands > 10 {
                    return Ok(());
                }
            }
        }
    }
}

/// `FROM:<a@b> SIZE=123` → ("a@b", ["SIZE=123"])
fn parse_path(arg: &str, prefix: &str) -> Option<(String, Vec<String>)> {
    let rest = arg.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| &arg[prefix.len()..])?.trim_start();
    let (path, params) = if let Some(r) = rest.strip_prefix('<') {
        let end = r.find('>')?;
        (&r[..end], &r[end + 1..])
    } else {
        rest.split_once(' ').unwrap_or((rest, ""))
    };
    // Source routes (@a,@b:user@c) are ignored.
    let path = path.rsplit(':').next().unwrap_or(path).trim();
    Some((path.to_ascii_lowercase(), params.split_whitespace().map(str::to_string).collect()))
}

impl Session {
    fn reset(&mut self) {
        self.mail_from = None;
        self.rcpts.clear();
    }

    fn ip(&self) -> IpAddr {
        self.peer.ip()
    }

    async fn auth(&self, conn: &mut Conn, arg: &str) -> std::io::Result<Option<User>> {
        let (mech, initial) = match arg.split_once(' ') {
            Some((m, i)) => (m.to_ascii_uppercase(), Some(i.trim().to_string())),
            None => (arg.to_ascii_uppercase(), None),
        };
        let decode = |s: &str| B64.decode(s.trim()).ok().map(|v| String::from_utf8_lossy(&v).into_owned());
        let (login, password) = match mech.as_str() {
            "PLAIN" => {
                let data = match initial.filter(|i| i != "=") {
                    Some(i) => i,
                    None => {
                        conn.reply("334 ").await?;
                        conn.read_line(4096).await?.unwrap_or_default()
                    }
                };
                let Some(decoded) = decode(&data) else { return Ok(None) };
                let mut parts = decoded.split('\0');
                let _authzid = parts.next();
                let (Some(l), Some(p)) = (parts.next(), parts.next()) else { return Ok(None) };
                (l.to_string(), p.to_string())
            }
            "LOGIN" => {
                let user = match initial {
                    Some(i) => i,
                    None => {
                        conn.reply("334 VXNlcm5hbWU6").await?;
                        conn.read_line(4096).await?.unwrap_or_default()
                    }
                };
                conn.reply("334 UGFzc3dvcmQ6").await?;
                let pass = conn.read_line(4096).await?.unwrap_or_default();
                (decode(&user).unwrap_or_default(), decode(&pass).unwrap_or_default())
            }
            _ => {
                conn.reply("504 5.5.4 Unrecognized authentication type").await?;
                return Ok(None);
            }
        };
        let store = self.app.store.clone();
        let ip = self.ip();
        let res = tokio::task::spawn_blocking(move || auth::login_app(&store, &login, &password, Some(ip))).await;
        // Submission is a mail function, so an account without a mailbox can't use it.
        Ok(res.ok().and_then(|r| r.ok()).filter(|u| u.mailbox))
    }

    fn received_header(&self, tls: bool, rcpt: Option<&str>) -> (String, String) {
        let with = match (tls, self.user.is_some()) {
            (true, true) => "ESMTPSA",
            (true, false) => "ESMTPS",
            (false, true) => "ESMTPA",
            (false, false) => "ESMTP",
        };
        let id = uuid::Uuid::new_v4().simple().to_string();
        let for_ = rcpt.map(|r| format!("\r\n\tfor <{r}>")).unwrap_or_default();
        (
            "Received".to_string(),
            format!(
                "from {} ([{}])\r\n\tby {} (Ondura Server) with {with} id {}{for_};\r\n\t{}",
                self.helo.as_deref().unwrap_or("unknown"),
                self.ip(),
                self.app.hostname(),
                &id[..16],
                mime::rfc2822_now()
            ),
        )
    }

    /// Handle a complete message; returns the SMTP reply.
    async fn accept_message(&mut self, raw: Vec<u8>, tls: bool) -> String {
        let rcpts = std::mem::take(&mut self.rcpts);
        let mail_from = self.mail_from.clone().unwrap_or_default();
        if let Some(user) = self.user.clone() {
            let received = self.received_header(tls, None);
            let raw = mime::prepend_headers(&raw, &[received]);
            let addresses: Vec<String> = rcpts.iter().map(|r| r.address.clone()).collect();
            return match submit::send(&self.app, &user, raw, submit::Options { envelope_from: Some(mail_from), recipients: Some(addresses), save_copy: false }).await {
                Ok(()) => "250 2.0.0 OK queued".into(),
                Err(e) => {
                    warn!(user = %user.email, "submission rejected: {e:#}");
                    format!("550 5.7.1 {}", first_line(&e.to_string()))
                }
            };
        }

        // Inbound from another server.
        let single = (rcpts.len() == 1).then(|| rcpts[0].address.as_str());
        let mut headers = vec![self.received_header(tls, single)];
        let mut junk = false;
        if self.app.cfg.smtp.verify_incoming
            && let Some(dns) = &self.app.dns
        {
            let verdict = verify_message(dns, &raw, self.ip(), self.helo.as_deref().unwrap_or(""), &mail_from, self.app.hostname()).await;
            headers.push(("Authentication-Results".into(), verdict.header));
            if verdict.reject && self.app.cfg.smtp.enforce_dmarc {
                info!(from = %mail_from, peer = %self.peer, "rejected by DMARC policy");
                return "550 5.7.1 Rejected by the DMARC policy of the sender domain".into();
            }
            junk |= verdict.quarantine;
        }
        if let Some(url) = &self.app.cfg.spam.rspamd {
            match spam::check(url, &raw, self.ip(), self.helo.as_deref().unwrap_or(""), &mail_from, &rcpts.iter().map(|r| r.address.clone()).collect::<Vec<_>>(), &self.app.cfg.spam).await {
                Ok(spam::Verdict::Reject) => return "550 5.7.1 Message rejected as spam".into(),
                Ok(spam::Verdict::Junk(score)) => {
                    junk = true;
                    headers.push(("X-Spam-Score".into(), format!("{score:.1}")));
                    headers.push(("X-Spam-Flag".into(), "YES".into()));
                }
                Ok(spam::Verdict::Clean(score)) => headers.push(("X-Spam-Score".into(), format!("{score:.1}"))),
                Err(e) => warn!("rspamd check failed: {e:#}"),
            }
        }
        let raw = mime::prepend_headers(&raw, &headers);
        let mut delivered = std::collections::HashSet::new();
        for r in &rcpts {
            let Some(uid) = r.user_id else { continue };
            if !delivered.insert(uid) {
                continue;
            }
            if let Err(e) = delivery::deliver(&self.app, uid, &raw, delivery::Options { junk, envelope_from: mail_from.clone() }).await {
                warn!(rcpt = %r.address, "local delivery failed: {e:#}");
                return "451 4.3.0 Local delivery failed, try again later".into();
            }
        }
        info!(from = %mail_from, rcpts = rcpts.len(), junk, "message received");
        "250 2.0.0 OK".into()
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

pub struct Verdict {
    pub header: String,
    pub reject: bool,
    pub quarantine: bool,
}

/// SPF, DKIM and DMARC checks of an incoming message.
pub async fn verify_message(dns: &mail_auth::MessageAuthenticator, raw: &[u8], ip: IpAddr, helo: &str, mail_from: &str, hostname: &str) -> Verdict {
    use mail_auth::dmarc::Policy;
    use mail_auth::{AuthenticatedMessage, AuthenticationResults, DmarcResult, dmarc::verify::DmarcParameters, spf::verify::SpfParameters};
    let Some(msg) = AuthenticatedMessage::parse(raw) else {
        return Verdict { header: format!("{hostname}; none"), reject: false, quarantine: false };
    };
    let sender = if mail_from.is_empty() { format!("postmaster@{helo}") } else { mail_from.to_string() };
    let spf = dns.verify_spf(SpfParameters::verify_mail_from(ip, helo, hostname, &sender)).await;
    let dkim = dns.verify_dkim(&msg).await;
    let from_domain = sender.rsplit_once('@').map(|(_, d)| d).unwrap_or(helo);
    let dmarc = dns.verify_dmarc(DmarcParameters::new(&msg, &dkim, from_domain, &spf)).await;
    let header_from = msg.from.first().cloned().unwrap_or_default();
    let header = AuthenticationResults::new(hostname)
        .with_spf_mailfrom_result(&spf, ip, &sender, helo)
        .with_dkim_results(&dkim, &header_from)
        .with_dmarc_result(&dmarc)
        .to_string();
    let header = header.trim_start_matches("Authentication-Results:").trim().trim_end_matches("\r\n").to_string();
    let failed = matches!(dmarc.result(), DmarcResult::Fail(_));
    Verdict { header, reject: failed && dmarc.policy() == Policy::Reject, quarantine: failed && dmarc.policy() == Policy::Quarantine }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(parse_path("FROM:<Max@Example.com> SIZE=100", "FROM:").unwrap(), ("max@example.com".into(), vec!["SIZE=100".into()]));
        assert_eq!(parse_path("from: <>", "FROM:").unwrap().0, "");
        assert_eq!(parse_path("TO:<@relay:anna@x.de>", "TO:").unwrap().0, "anna@x.de");
        assert!(parse_path("TO anna", "TO:").is_none());
    }
}
