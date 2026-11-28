//! Password hashing and login checks.
//!
//! ActiveSync and EWS clients send credentials with every request, so
//! successful logins are cached for a few minutes (keyed by a salted hash of
//! the password) to avoid running Argon2 on each request. Failed logins are
//! rate limited per login name and client address.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::store::{Store, User};

const CACHE_TTL: Duration = Duration::from_secs(300);
const MAX_FAILURES: u32 = 10;
/// Global cap on failures from one IP across all accounts, to blunt password
/// spraying (many accounts, few guesses each) from a single source.
const MAX_IP_FAILURES: u32 = 50;
const FAILURE_WINDOW: Duration = Duration::from_secs(900);

pub fn hash_password(password: &str) -> Result<String> {
    if password.is_empty() {
        return Err(anyhow!("password must not be empty"));
    }
    let hash = Argon2::default().hash_password(password.as_bytes()).map_err(|e| anyhow!("password hashing failed: {e}"))?;
    Ok(hash.to_string())
}

pub fn verify_hash(password: &str, hash: &str) -> bool {
    Argon2::default().verify_password(password.as_bytes(), hash).is_ok()
}

struct State {
    secret: [u8; 32],
    /// user id → (password digest, expires)
    cache: HashMap<i64, (Vec<u8>, Instant)>,
    /// (login, ip) → (failures, first failure)
    failures: HashMap<(String, Option<IpAddr>), (u32, Instant)>,
    /// ip → (failures across all accounts, first failure)
    ip_failures: HashMap<IpAddr, (u32, Instant)>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        let mut secret = [0u8; 32];
        rand::fill(&mut secret);
        Mutex::new(State { secret, cache: HashMap::new(), failures: HashMap::new(), ip_failures: HashMap::new() })
    })
}

fn digest(secret: &[u8], password: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(secret);
    h.update(password.as_bytes());
    h.finalize().to_vec()
}

/// Drop cached logins of a user (password change, disabled account).
pub fn forget(user_id: i64) {
    state().lock().unwrap_or_else(|e| e.into_inner()).cache.remove(&user_id);
}

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("invalid user name or password")]
    Invalid,
    #[error("too many failed logins, try again later")]
    Locked,
    #[error("account disabled")]
    Disabled,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Check a login (address, alias or unique local part) and password.
pub fn login(store: &Store, login: &str, password: &str, ip: Option<IpAddr>) -> std::result::Result<User, LoginError> {
    let key = (login.trim().to_ascii_lowercase(), ip);
    {
        let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
        st.failures.retain(|_, (_, first)| first.elapsed() < FAILURE_WINDOW);
        st.ip_failures.retain(|_, (_, first)| first.elapsed() < FAILURE_WINDOW);
        if st.failures.get(&key).is_some_and(|(n, _)| *n >= MAX_FAILURES) {
            return Err(LoginError::Locked);
        }
        if let Some(ip) = ip
            && st.ip_failures.get(&ip).is_some_and(|(n, _)| *n >= MAX_IP_FAILURES)
        {
            return Err(LoginError::Locked);
        }
    }
    let user = store.user_for_login(login)?;
    let Some(user) = user else {
        // Same cost as a real check, so user names cannot be probed by timing.
        let _ = verify_hash(password, "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$Hl4SJ1t3H6Lw3cY5u9iV7w1J3oQ2mXgWl1M9cYV7a8s");
        fail(key);
        return Err(LoginError::Invalid);
    };
    if !user.enabled {
        return Err(LoginError::Disabled);
    }
    if user.kind != "user" {
        // Shared and room mailboxes are opened with delegate rights only.
        fail(key);
        return Err(LoginError::Invalid);
    }
    {
        let st = state().lock().unwrap_or_else(|e| e.into_inner());
        let d = digest(&st.secret, password);
        if let Some((cached, expires)) = st.cache.get(&user.id)
            && *cached == d
            && *expires > Instant::now()
        {
            return Ok(user);
        }
    }
    if verify_hash(password, &user.password) {
        let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
        let d = digest(&st.secret, password);
        st.cache.insert(user.id, (d, Instant::now() + CACHE_TTL));
        st.failures.remove(&key);
        if let Some(ip) = ip {
            st.ip_failures.remove(&ip);
        }
        Ok(user)
    } else {
        fail(key);
        Err(LoginError::Invalid)
    }
}

/// Bring an app password to its stored form: they are shown in groups
/// (`abcd efgh ijkl mnop`), and apps may keep the spaces or dashes.
pub fn normalize_app_password(p: &str) -> String {
    p.chars().filter(|c| !c.is_whitespace() && *c != '-').collect::<String>().to_ascii_lowercase()
}

/// Sign-in for mail and sync apps (IMAP, POP3, SMTP, ActiveSync, EWS, the
/// Nextcloud and Ondura clients): an app password of the account, or the
/// account password unless the account allows only app passwords there.
/// Browser sign-ins use [`login`] (and a second step where it is turned on).
pub fn login_app(store: &Store, login_name: &str, password: &str, ip: Option<IpAddr>) -> std::result::Result<User, LoginError> {
    let key = (login_name.trim().to_ascii_lowercase(), ip);
    {
        let st = state().lock().unwrap_or_else(|e| e.into_inner());
        if st.failures.get(&key).is_some_and(|(n, first)| *n >= MAX_FAILURES && first.elapsed() < FAILURE_WINDOW) {
            return Err(LoginError::Locked);
        }
    }
    let user = store.user_for_login(login_name)?;
    let app_pw = normalize_app_password(password);
    if let Some(u) = &user
        && app_pw.len() >= 16
        && store.app_password_user(&app_pw).ok().flatten() == Some(u.id)
    {
        if !u.enabled {
            return Err(LoginError::Disabled);
        }
        if u.kind == "user" {
            store.touch_app_password(&app_pw);
            return Ok(u.clone());
        }
    }
    if user.as_ref().is_some_and(|u| store.settings(u.id).map(|s| s.app_passwords_only).unwrap_or(false)) {
        fail(key);
        return Err(LoginError::Invalid);
    }
    login(store, login_name, password, ip)
}

fn fail(key: (String, Option<IpAddr>)) {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    let ip = key.1;
    st.failures.entry(key).or_insert((0, Instant::now())).0 += 1;
    if let Some(ip) = ip {
        st.ip_failures.entry(ip).or_insert((0, Instant::now())).0 += 1;
    }
}

/// Parse an HTTP `Authorization: Basic …` header into (login, password).
pub fn parse_basic(header: &str) -> Option<(String, String)> {
    let b64 = header.trim().strip_prefix("Basic ").or_else(|| header.trim().strip_prefix("basic "))?;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (user, pass) = s.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

/// Random password for new mailboxes created without one.
pub fn random_password() -> String {
    const CHARS: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    bytes.iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_flow() {
        let (s, u) = crate::store::test_store();
        assert_eq!(login(&s, "niklas@example.com", "secret", None).unwrap().id, u.id);
        // Cached path.
        assert_eq!(login(&s, "NIKLAS@example.com", "secret", None).unwrap().id, u.id);
        assert!(matches!(login(&s, "niklas@example.com", "wrong", None), Err(LoginError::Invalid)));
        s.set_password(u.id, "neu").unwrap();
        assert!(login(&s, "niklas@example.com", "secret", None).is_err());
        assert!(login(&s, "niklas", "neu", None).is_ok());
        assert_eq!(parse_basic("Basic bmlrbGFzOnNlY3JldA==").unwrap(), ("niklas".into(), "secret".into()));
    }

    #[test]
    fn global_ip_cap_blunts_spraying() {
        let (s, _u) = crate::store::test_store();
        let ip: IpAddr = "198.51.100.7".parse().unwrap();
        // Fail against many distinct (non-existent) accounts from one IP; each
        // stays under the per-account cap but trips the global per-IP cap.
        for n in 0..MAX_IP_FAILURES {
            let who = format!("ghost{n}@example.com");
            assert!(matches!(login(&s, &who, "x", Some(ip)), Err(LoginError::Invalid)));
        }
        // The next attempt from this IP is locked even for a fresh account.
        assert!(matches!(login(&s, "another@example.com", "x", Some(ip)), Err(LoginError::Locked)));
        // A different IP is unaffected.
        let ip2: IpAddr = "203.0.113.9".parse().unwrap();
        assert!(matches!(login(&s, "another@example.com", "x", Some(ip2)), Err(LoginError::Invalid)));
    }
}
