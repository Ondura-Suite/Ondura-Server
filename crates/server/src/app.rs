//! Shared state of the running server.

use std::sync::{Arc, RwLock};

use rustls::ServerConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::config::Config;
use crate::store::Store;

/// Operational settings an admin can change at runtime from the Admin Center.
/// Each is an override; `None` means "use the value from the configuration
/// file". Persisted in the store's `meta` table so they survive restarts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SettingsOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activesync_provisioning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activesync_require_password: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collab_enabled: Option<bool>,
}

/// Key under which the overrides are stored in `meta`.
const SETTINGS_KEY: &str = "server_settings";

pub struct App {
    pub cfg: Config,
    pub store: Store,
    /// Runtime settings overrides (see [`SettingsOverrides`]).
    pub settings: RwLock<SettingsOverrides>,
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
    /// TLS configuration for IMAP/POP3 (shares the SMTP server certificate).
    pub fn imap_tls(&self) -> Option<Arc<ServerConfig>> {
        self.smtp_tls.clone()
    }

    pub fn hostname(&self) -> &str {
        &self.cfg.hostname
    }

    fn overrides(&self) -> SettingsOverrides {
        self.settings.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    // --- effective settings: the runtime override, else the config value ---

    pub fn activesync_provisioning(&self) -> bool {
        self.overrides().activesync_provisioning.unwrap_or(self.cfg.activesync.provisioning)
    }
    pub fn activesync_require_password(&self) -> bool {
        self.overrides().activesync_require_password.unwrap_or(self.cfg.activesync.require_password)
    }
    pub fn collab_enabled(&self) -> bool {
        self.overrides().collab_enabled.unwrap_or(self.cfg.collab.enabled)
    }

    /// Load persisted overrides from the store into memory (called at startup).
    pub fn load_settings(&self) {
        if let Ok(Some(raw)) = self.store.get_meta(SETTINGS_KEY)
            && let Ok(o) = serde_json::from_str::<SettingsOverrides>(&raw)
        {
            *self.settings.write().unwrap_or_else(|e| e.into_inner()) = o;
        }
    }

    /// Apply a patch of settings and persist it. Only the fields present in the
    /// patch change; the rest keep their current value.
    pub fn update_settings(&self, patch: &SettingsOverrides) -> anyhow::Result<()> {
        {
            let mut g = self.settings.write().unwrap_or_else(|e| e.into_inner());
            if patch.activesync_provisioning.is_some() {
                g.activesync_provisioning = patch.activesync_provisioning;
            }
            if patch.activesync_require_password.is_some() {
                g.activesync_require_password = patch.activesync_require_password;
            }
            if patch.collab_enabled.is_some() {
                g.collab_enabled = patch.collab_enabled;
            }
        }
        let raw = serde_json::to_string(&self.overrides())?;
        self.store.set_meta(SETTINGS_KEY, &raw)?;
        Ok(())
    }

    /// Settings for the Admin Center: the editable (live) values, plus the
    /// read-only infrastructure values that require editing the config file.
    pub fn settings_json(&self) -> Value {
        json!({
            "editable": {
                "activesyncProvisioning": self.activesync_provisioning(),
                "activesyncRequirePassword": self.activesync_require_password(),
                "collabEnabled": self.collab_enabled(),
            },
            "readonly": {
                "hostname": self.cfg.hostname,
                "timeZone": self.cfg.time_zone,
                "backend": format!("{:?}", self.cfg.database.backend).to_ascii_lowercase(),
                "webmail": self.cfg.http.webmail,
                "adminCenter": self.cfg.http.admin_center,
                "maxMessageSize": self.cfg.smtp.max_message_size,
                "relay": self.cfg.smtp.relay.is_some(),
                "spamFilter": self.cfg.spam.rspamd.is_some(),
                "verifyIncoming": self.cfg.smtp.verify_incoming,
            },
        })
    }

    /// App for tests: in-memory store, no TLS, no DNS.
    #[cfg(test)]
    pub fn test() -> (Shared, crate::store::User) {
        let (store, user) = crate::store::test_store();
        let cfg: Config = toml::from_str("hostname = \"mail.example.com\"\n").expect("config");
        (Arc::new(App { cfg, store, settings: RwLock::new(SettingsOverrides::default()), smtp_tls: None, https_tls: None, dns: None, queue_wake: Notify::new() }), user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_override_persists_and_reloads() {
        let (app, _u) = App::test();
        let before = app.collab_enabled();

        // Overriding flips the effective value.
        app.update_settings(&SettingsOverrides { collab_enabled: Some(!before), ..Default::default() }).unwrap();
        assert_eq!(app.collab_enabled(), !before);

        // A patch touches only the fields it sets.
        app.update_settings(&SettingsOverrides { activesync_provisioning: Some(false), ..Default::default() }).unwrap();
        assert_eq!(app.collab_enabled(), !before);
        assert!(!app.activesync_provisioning());

        // The override was persisted to the store and reloads after a restart.
        *app.settings.write().unwrap() = SettingsOverrides::default();
        assert_eq!(app.collab_enabled(), before); // back to config default in memory
        app.load_settings();
        assert_eq!(app.collab_enabled(), !before); // restored from the store
        assert!(!app.activesync_provisioning());
    }
}
