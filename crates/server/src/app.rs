//! Shared state of the running server.

use std::sync::Arc;

use rustls::ServerConfig;
use tokio::sync::Notify;

use crate::config::Config;
use crate::store::Store;

pub struct App {
    pub cfg: Config,
    pub store: Store,
    /// TLS for SMTP (no ALPN).
    pub smtp_tls: Option<Arc<ServerConfig>>,
    /// TLS for HTTPS (ALPN h2 + http/1.1).
    pub https_tls: Option<Arc<ServerConfig>>,
    /// DNS for SPF/DKIM/DMARC and MX lookups.
    pub dns: Option<mail_auth::MessageAuthenticator>,
    /// Wakes the outbound queue worker.
    pub queue_wake: Notify,
}

pub type Shared = Arc<App>;

impl App {
    pub fn hostname(&self) -> &str {
        &self.cfg.hostname
    }

    /// App for tests: in-memory store, no TLS, no DNS.
    #[cfg(test)]
    pub fn test() -> (Shared, crate::store::User) {
        let (store, user) = crate::store::test_store();
        let cfg: Config = toml::from_str("hostname = \"mail.example.com\"\n").expect("config");
        (Arc::new(App { cfg, store, smtp_tls: None, https_tls: None, dns: None, queue_wake: Notify::new() }), user)
    }
}
