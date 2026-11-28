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
        "nextcloud": s.nextcloud,
        "cloudEnabled": app.cfg.cloud.enabled,
        "host": app.cfg.hostname,
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
    #[serde(default)]
    nextcloud: Option<bool>,
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
    if let Some(n) = b.nextcloud {
        s.nextcloud = n;
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

// ------------------------------------------------------- two-step sign-in

#[derive(Deserialize)]
struct PasswordOnly {
    password: String,
}

#[derive(Deserialize)]
struct CodeOnly {
    code: String,
}

fn check_password(user: &User, password: &str) -> Result<(), (axum::http::StatusCode, String)> {
    if !auth::verify_hash(password, &user.password) {
        return Err((axum::http::StatusCode::FORBIDDEN, "The password is incorrect.".into()));
    }
    Ok(())
}

/// GET /security/2fa — whether two-step sign-in is on, and recovery codes left.
pub fn two_step_status(app: &Shared, user: &User) -> ApiResult {
    Ok(json!({
        "enabled": app.store.totp_enabled(user.id).map_err(internal)?,
        "recoveryLeft": app.store.totp_recovery_left(user.id).map_err(internal)?,
    }))
}

/// POST /security/2fa/begin { password } — a new secret for the authenticator
/// app, as text and as an `otpauth://` link (the page shows it as a QR code).
pub fn two_step_begin(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: PasswordOnly = parse(body)?;
    check_password(user, &b.password)?;
    let secret = app.store.totp_begin(user.id).map_err(|e| bad(e.to_string()))?;
    let issuer = format!("Ondura {}", app.cfg.hostname);
    let uri = crate::totp::otpauth_uri(&issuer, &user.email, &secret);
    Ok(json!({ "secret": secret, "qr": crate::qr::svg(&uri), "uri": uri }))
}

/// POST /security/2fa/enable { code } — turn it on with a code from the app;
/// returns the recovery codes (shown once).
pub fn two_step_enable(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: CodeOnly = parse(body)?;
    let codes = app.store.totp_enable(user.id, &b.code).map_err(|e| bad(e.to_string()))?;
    app.store.audit(&user.email, "2fa.enable", "");
    Ok(json!({ "recovery": codes }))
}

/// POST /security/2fa/disable { password } — turn it off.
pub fn two_step_disable(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: PasswordOnly = parse(body)?;
    check_password(user, &b.password)?;
    app.store.totp_disable(user.id).map_err(internal)?;
    app.store.audit(&user.email, "2fa.disable", "");
    Ok(json!({ "ok": true }))
}

// ----------------------------------------------------------- app passwords

/// GET /security/app-passwords — the account's app passwords (not the
/// passwords themselves) and whether mail apps may only use them.
pub fn app_passwords(app: &Shared, user: &User) -> ApiResult {
    let items = app.store.app_passwords(user.id).map_err(internal)?;
    let only = app.store.settings(user.id).map_err(internal)?.app_passwords_only;
    Ok(json!({
        "items": items.iter().map(|(id, name, created, used)| json!({ "id": id, "name": name, "created": created, "lastUsed": used })).collect::<Vec<_>>(),
        "appPasswordsOnly": only,
    }))
}

/// POST /security/app-password { name } — a new app password, shown once.
pub fn create_app_password(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        #[serde(default)]
        name: String,
    }
    let b: B = parse(body)?;
    let pw = app.store.create_mail_app_password(user.id, &b.name).map_err(internal)?;
    app.store.audit(&user.email, "app-password.create", b.name.trim());
    Ok(json!({ "password": pw }))
}

/// POST /security/app-password/delete { id }
pub fn delete_app_password(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        id: String,
    }
    let b: B = parse(body)?;
    if !app.store.delete_app_password(user.id, &b.id).map_err(internal)? {
        return Err(super::not_found());
    }
    crate::auth::forget(user.id);
    Ok(json!({ "ok": true }))
}

/// POST /security/app-passwords-only { on } — whether mail and sync apps may
/// sign in only with an app password.
pub fn set_app_passwords_only(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        on: bool,
    }
    let b: B = parse(body)?;
    let mut s = app.store.settings(user.id).map_err(internal)?;
    s.app_passwords_only = b.on;
    app.store.set_settings(user.id, &s).map_err(internal)?;
    crate::auth::forget(user.id);
    Ok(json!({ "ok": true }))
}

/// POST /profile — the caller's own directory details.
pub fn save_profile(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let p: crate::directory::Profile = parse(body)?;
    let p = p.cleaned();
    if !p.manager.is_empty() && p.manager == user.email {
        return Err(bad("You can't be your own manager."));
    }
    app.store.set_profile(user.id, &p).map_err(internal)?;
    Ok(json!(p))
}
