//! Server configuration (`/etc/ondura-server/config.toml`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Public host name of the server (MX, EHLO, Autodiscover, TLS certificate).
    pub hostname: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Default time zone (IANA name) for calendar items without one.
    #[serde(default = "default_tz")]
    pub time_zone: String,
    #[serde(default)]
    pub tls: Tls,
    #[serde(default)]
    pub http: Http,
    #[serde(default)]
    pub smtp: Smtp,
    #[serde(default)]
    pub spam: Spam,
    #[serde(default)]
    pub activesync: ActiveSync,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    /// PEM certificate chain (e.g. Let's Encrypt `fullchain.pem`). Reloaded
    /// automatically when the file changes.
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Http {
    /// HTTPS listeners (ActiveSync, EWS, Autodiscover, Admin Center).
    #[serde(default = "default_https")]
    pub https: Vec<SocketAddr>,
    /// Plain HTTP listeners. Without `behind_proxy` they only redirect to HTTPS.
    #[serde(default)]
    pub http: Vec<SocketAddr>,
    /// Serve everything on the plain HTTP listeners because a reverse proxy
    /// terminates TLS in front of the server.
    #[serde(default)]
    pub behind_proxy: bool,
    /// Proxies whose `X-Forwarded-For` is trusted.
    #[serde(default)]
    pub trusted_proxies: Vec<std::net::IpAddr>,
    /// Allow the Admin Center (`/admin`).
    #[serde(default = "yes")]
    pub admin_center: bool,
    /// Allow webmail (`/mail`) for end users.
    #[serde(default = "yes")]
    pub webmail: bool,
}

impl Default for Http {
    fn default() -> Self {
        Http { https: default_https(), http: vec![], behind_proxy: false, trusted_proxies: vec![], admin_center: true, webmail: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Smtp {
    /// Port 25: mail from other servers to local mailboxes.
    #[serde(default = "default_inbound")]
    pub inbound: Vec<SocketAddr>,
    /// Port 587 (STARTTLS) for mail clients, authentication required.
    #[serde(default = "default_submission")]
    pub submission: Vec<SocketAddr>,
    /// Port 465 (implicit TLS) for mail clients, authentication required.
    #[serde(default = "default_submissions")]
    pub submissions: Vec<SocketAddr>,
    #[serde(default = "default_max_size")]
    pub max_message_size: usize,
    /// Send all outgoing mail through this host (`host:port`) instead of
    /// delivering directly via MX.
    #[serde(default)]
    pub relay: Option<String>,
    #[serde(default)]
    pub relay_username: Option<String>,
    #[serde(default)]
    pub relay_password: Option<String>,
    /// Check SPF, DKIM and DMARC of incoming mail.
    #[serde(default = "yes")]
    pub verify_incoming: bool,
    /// Reject mail failing DMARC with policy `reject` (otherwise it goes to junk).
    #[serde(default = "yes")]
    pub enforce_dmarc: bool,
    /// Days a message stays in the queue before it bounces.
    #[serde(default = "default_queue_days")]
    pub queue_lifetime_days: i64,
}

impl Default for Smtp {
    fn default() -> Self {
        Smtp {
            inbound: default_inbound(),
            submission: default_submission(),
            submissions: default_submissions(),
            max_message_size: default_max_size(),
            relay: None,
            relay_username: None,
            relay_password: None,
            verify_incoming: true,
            enforce_dmarc: true,
            queue_lifetime_days: default_queue_days(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spam {
    /// rspamd controller/normal worker URL, e.g. `http://127.0.0.1:11333`.
    pub rspamd: Option<String>,
    /// Score from which mail goes to the junk folder.
    #[serde(default)]
    pub junk_score: Option<f64>,
    /// Score from which mail is rejected.
    #[serde(default)]
    pub reject_score: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveSync {
    /// Require devices to accept the security policy (Provision).
    #[serde(default = "yes")]
    pub provisioning: bool,
    /// Require a device PIN/password.
    #[serde(default)]
    pub require_password: bool,
    #[serde(default = "default_min_password")]
    pub min_password_length: u32,
    /// Longest heartbeat for Ping/Sync wait, seconds.
    #[serde(default = "default_heartbeat")]
    pub max_heartbeat: u64,
}

impl Default for ActiveSync {
    fn default() -> Self {
        ActiveSync { provisioning: true, require_password: false, min_password_length: default_min_password(), max_heartbeat: default_heartbeat() }
    }
}

fn yes() -> bool {
    true
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ondura-server")
}
fn default_tz() -> String {
    "Europe/Berlin".into()
}
fn default_https() -> Vec<SocketAddr> {
    vec!["[::]:443".parse().expect("address")]
}
fn default_inbound() -> Vec<SocketAddr> {
    vec!["[::]:25".parse().expect("address")]
}
fn default_submission() -> Vec<SocketAddr> {
    vec!["[::]:587".parse().expect("address")]
}
fn default_submissions() -> Vec<SocketAddr> {
    vec!["[::]:465".parse().expect("address")]
}
fn default_max_size() -> usize {
    50 * 1024 * 1024
}
fn default_queue_days() -> i64 {
    5
}
fn default_min_password() -> u32 {
    4
}
fn default_heartbeat() -> u64 {
    1680
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.hostname.trim().is_empty() || !self.hostname.contains('.') {
            bail!("hostname must be the fully qualified name of the server, e.g. mail.example.com");
        }
        if self.time_zone.parse::<chrono_tz::Tz>().is_err() {
            bail!("unknown time_zone {:?} (use an IANA name like Europe/Berlin)", self.time_zone);
        }
        if self.tls.cert.is_some() != self.tls.key.is_some() {
            bail!("tls.cert and tls.key must be set together");
        }
        Ok(())
    }

    pub fn database(&self) -> PathBuf {
        self.data_dir.join("ondura.db")
    }

    /// Example configuration written by `ondura-server init`.
    pub fn example(hostname: &str) -> String {
        format!(
            r#"# Ondura Server configuration. See docs/admin.md for all options.

# Public name of the server: MX target, Autodiscover host, TLS certificate name.
hostname = "{hostname}"
data_dir = "/var/lib/ondura-server"
time_zone = "Europe/Berlin"

[tls]
# Certificate and key in PEM format, e.g. from Let's Encrypt (certbot).
# Changed files are picked up automatically.
cert = "/etc/letsencrypt/live/{hostname}/fullchain.pem"
key = "/etc/letsencrypt/live/{hostname}/privkey.pem"

[http]
https = ["[::]:443"]
# Plain HTTP only redirects to HTTPS, unless behind_proxy = true.
http = ["[::]:80"]
behind_proxy = false
admin_center = true
# Browser webmail for end users at /mail.
webmail = true

[smtp]
inbound = ["[::]:25"]
submission = ["[::]:587"]
submissions = ["[::]:465"]
max_message_size = 52428800
verify_incoming = true
# relay = "smtp.provider.example:587"

[spam]
# rspamd = "http://127.0.0.1:11333"

[activesync]
provisioning = true
require_password = false
"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let cfg: Config = toml::from_str(&Config::example("mail.example.com")).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.smtp.submission[0].port(), 587);
        assert!(cfg.activesync.provisioning);
    }
}
