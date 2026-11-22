//! Account settings: display name, signature, out-of-office, forwarding,
//! and self-service password change.

use axum::body::Bytes;
use serde::Deserialize;
use serde_json::json;

use super::{bad, internal, parse, ApiResult};
use crate::app::Shared;
use crate::auth;
use crate::store::User;

/// GET /settings
pub fn get(app: &Shared, user: &User) -> ApiResult {
    let s = app.store.settings(user.id).map_err(internal)?;
    Ok(json!({
        "name": user.name,
        "email": user.email,
        "signature": s.signature,
        "oof": {
            "state": s.oof.state,
            "internalReply": s.oof.internal_reply,
            "externalReply": s.oof.external_reply,
            "externalAudience": s.oof.external_audience,
            "start": s.oof.start,
            "end": s.oof.end,
        },
        "forwardTo": s.forward_to,
        "keepForwardedCopy": s.keep_forwarded_copy,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Oof {
    #[serde(default)]
    state: String,
    #[serde(default)]
    internal_reply: String,
    #[serde(default)]
    external_reply: String,
    #[serde(default)]
    external_audience: String,
    #[serde(default)]
    start: Option<i64>,
    #[serde(default)]
    end: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    oof: Option<Oof>,
    #[serde(default)]
    forward_to: Option<String>,
    #[serde(default)]
    keep_forwarded_copy: Option<bool>,
}

/// POST /settings
pub fn save(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: SettingsBody = parse(body)?;
    if let Some(name) = &b.name {
        let name = name.trim();
        if !name.is_empty() {
            app.store.update_user(user.id, Some(name), None, None, None).map_err(internal)?;
        }
    }
    let mut s = app.store.settings(user.id).map_err(internal)?;
    if let Some(sig) = b.signature {
        s.signature = sig;
    }
    if let Some(o) = b.oof {
        if !["Disabled", "Enabled", "Scheduled"].contains(&o.state.as_str()) {
            return Err(bad("invalid out-of-office state"));
        }
        if !["None", "Known", "All"].contains(&o.external_audience.as_str()) && !o.external_audience.is_empty() {
            return Err(bad("invalid audience"));
        }
        s.oof.state = o.state;
        s.oof.internal_reply = o.internal_reply;
        s.oof.external_reply = o.external_reply;
        s.oof.external_audience = if o.external_audience.is_empty() { "All".into() } else { o.external_audience };
        s.oof.start = o.start;
        s.oof.end = o.end;
    }
    if let Some(f) = b.forward_to {
        let f = f.trim();
        s.forward_to = if f.is_empty() { None } else { Some(f.to_string()) };
    }
    if let Some(k) = b.keep_forwarded_copy {
        s.keep_forwarded_copy = k;
    }
    app.store.set_settings(user.id, &s).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// POST /password — verify the current password, then set a new one.
pub fn password(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PwBody {
        current: String,
        new: String,
    }
    let b: PwBody = parse(body)?;
    if !auth::verify_hash(&b.current, &user.password) {
        return Err((axum::http::StatusCode::FORBIDDEN, "The current password is incorrect.".into()));
    }
    let min = app.cfg.activesync.min_password_length.max(1) as usize;
    if b.new.chars().count() < min {
        return Err(bad(format!("The new password must be at least {min} characters.")));
    }
    app.store.set_password(user.id, &b.new).map_err(internal)?;
    Ok(json!({ "ok": true }))
}
