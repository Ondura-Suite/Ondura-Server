//! Two-step sign-in with time-based one-time codes (TOTP, RFC 6238): six
//! digits from an authenticator app, a new one every 30 seconds, HMAC-SHA1 as
//! every common authenticator app expects.
//!
//! An account turns it on in two steps: `totp_begin` stores a new secret (not
//! yet active), and `totp_enable` activates it once the user has entered a
//! valid code from the app, returning ten one-time recovery codes. From then
//! on, signing in to the web app and the Admin Center with the account password
//! also needs a code (or a recovery code).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use ring::hmac;
use sha2::{Digest, Sha256};

use crate::db::{params, OptionalExtension};
use crate::store::{now, Store};

const STEP: i64 = 30;
const DIGITS: u32 = 6;
const RECOVERY_CODES: usize = 10;
/// Wrong codes allowed per account within `LOCK_WINDOW` before codes are
/// refused for the rest of the window.
const MAX_FAILURES: u32 = 5;
const LOCK_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Recent wrong codes, keyed by the account's secret (unique per setup).
static FAILURES: Mutex<Option<HashMap<String, (u32, Instant)>>> = Mutex::new(None);

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let v = B32.iter().position(|&x| x as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

/// The code for one time step (RFC 4226 HOTP with the step as counter).
pub fn code_at(secret: &[u8], step: i64) -> u32 {
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret);
    let tag = hmac::sign(&key, &(step as u64).to_be_bytes());
    let h = tag.as_ref();
    let off = (h[h.len() - 1] & 0x0f) as usize;
    let bin = u32::from_be_bytes([h[off] & 0x7f, h[off + 1], h[off + 2], h[off + 3]]);
    bin % 10u32.pow(DIGITS)
}

fn hash_recovery(code: &str) -> String {
    let norm: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    hex::encode(Sha256::digest(norm.as_bytes()))
}

/// The `otpauth://` URI authenticator apps import (also as a QR code).
pub fn otpauth_uri(issuer: &str, account: &str, secret_b32: &str) -> String {
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~@".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    format!("otpauth://totp/{}:{}?secret={secret_b32}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP}", enc(issuer), enc(account), enc(issuer))
}

fn locked(key: &str) -> bool {
    let mut g = FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    map.retain(|_, (_, t)| t.elapsed() < LOCK_WINDOW);
    map.get(key).is_some_and(|(n, _)| *n >= MAX_FAILURES)
}

fn record(key: &str, ok: bool) {
    let mut g = FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    if ok {
        map.remove(key);
    } else {
        let e = map.entry(key.to_string()).or_insert((0, Instant::now()));
        e.0 += 1;
    }
}

/// Outcome of checking a sign-in code.
#[derive(Debug, PartialEq, Eq)]
pub enum CodeCheck {
    Ok,
    Wrong,
    /// Too many wrong codes recently.
    Locked,
}

impl Store {
    /// Whether the account signs in with a code as well as its password.
    pub fn totp_enabled(&self, user_id: i64) -> Result<bool> {
        let v: Option<i64> = self.db().query_row("SELECT enabled FROM totp WHERE user_id = ?1", [user_id], |r| r.get(0)).optional()?;
        Ok(v == Some(1))
    }

    /// How many unused recovery codes are left.
    pub fn totp_recovery_left(&self, user_id: i64) -> Result<usize> {
        let v: Option<String> = self.db().query_row("SELECT recovery FROM totp WHERE user_id = ?1", [user_id], |r| r.get(0)).optional()?;
        Ok(v.map(|s| s.lines().filter(|l| !l.is_empty()).count()).unwrap_or(0))
    }

    /// Start setting up: a fresh secret, stored but not active yet. Returns it
    /// base32-encoded. Refused while two-step sign-in is on.
    pub fn totp_begin(&self, user_id: i64) -> Result<String> {
        if self.totp_enabled(user_id)? {
            bail!("two-step sign-in is already on");
        }
        let mut secret = [0u8; 20];
        rand::fill(&mut secret);
        let b32 = base32_encode(&secret);
        self.db().execute(
            "INSERT INTO totp (user_id, secret, enabled, recovery, last_step) VALUES (?1, ?2, 0, '', 0) \
             ON CONFLICT (user_id) DO UPDATE SET secret = excluded.secret, enabled = 0, recovery = '', last_step = 0",
            params![user_id, &b32],
        )?;
        Ok(b32)
    }

    /// Finish setting up with a code from the app. Returns the recovery codes,
    /// which are shown once and stored only as hashes.
    pub fn totp_enable(&self, user_id: i64, code: &str) -> Result<Vec<String>> {
        let secret: Option<(String, i64)> = self.db().query_row("SELECT secret, enabled FROM totp WHERE user_id = ?1", [user_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((secret, 0)) = secret else { bail!("start the setup first") };
        let Some(step) = self.matching_step(&secret, code, 0) else { bail!("the code is not correct") };
        let codes: Vec<String> = (0..RECOVERY_CODES)
            .map(|_| {
                let mut b = [0u8; 5];
                rand::fill(&mut b);
                let s = base32_encode(&b).to_ascii_lowercase();
                format!("{}-{}", &s[..4], &s[4..8])
            })
            .collect();
        let hashes: Vec<String> = codes.iter().map(|c| hash_recovery(c)).collect();
        self.db().execute("UPDATE totp SET enabled = 1, recovery = ?1, last_step = ?2 WHERE user_id = ?3", params![hashes.join("\n"), step, user_id])?;
        Ok(codes)
    }

    pub fn totp_disable(&self, user_id: i64) -> Result<()> {
        self.db().execute("DELETE FROM totp WHERE user_id = ?1", [user_id])?;
        Ok(())
    }

    /// The step a code belongs to, within one step of now and newer than
    /// `after` (so a code can't be used twice).
    fn matching_step(&self, secret_b32: &str, code: &str, after: i64) -> Option<i64> {
        let code = code.trim().replace(' ', "");
        if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let want: u32 = code.parse().ok()?;
        let secret = base32_decode(secret_b32)?;
        let cur = now() / STEP;
        (cur - 1..=cur + 1).find(|s| *s > after && code_at(&secret, *s) == want)
    }

    /// Check a sign-in code: a current code from the app, or an unused
    /// recovery code (which is then used up).
    pub fn totp_check(&self, user_id: i64, code: &str) -> Result<CodeCheck> {
        let row: Option<(String, String, i64)> = self
            .db()
            .query_row("SELECT secret, recovery, last_step FROM totp WHERE user_id = ?1 AND enabled = 1", [user_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let Some((secret, recovery, last)) = row else { return Ok(CodeCheck::Ok) };
        if locked(&secret) {
            return Ok(CodeCheck::Locked);
        }
        if let Some(step) = self.matching_step(&secret, code, last) {
            self.db().execute("UPDATE totp SET last_step = ?1 WHERE user_id = ?2", params![step, user_id])?;
            record(&secret, true);
            return Ok(CodeCheck::Ok);
        }
        let h = hash_recovery(code);
        if code.trim().len() >= 8 && recovery.lines().any(|l| l == h) {
            let rest: Vec<&str> = recovery.lines().filter(|l| *l != h && !l.is_empty()).collect();
            self.db().execute("UPDATE totp SET recovery = ?1 WHERE user_id = ?2", params![rest.join("\n"), user_id])?;
            record(&secret, true);
            return Ok(CodeCheck::Ok);
        }
        record(&secret, false);
        Ok(CodeCheck::Wrong)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    #[test]
    fn rfc6238_reference_codes() {
        // RFC 6238 appendix B, SHA-1 key "12345678901234567890" (8 digits there;
        // the last six digits are the 6-digit code).
        let key = b"12345678901234567890";
        assert_eq!(code_at(key, 59 / 30), 287082);
        assert_eq!(code_at(key, 1111111109 / 30), 81804);
        assert_eq!(code_at(key, 2000000000 / 30), 279037);
    }

    #[test]
    fn base32_round_trip() {
        let data = b"\x00\x01hello world\xff";
        assert_eq!(base32_decode(&base32_encode(data)).unwrap(), data);
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn setup_sign_in_codes_and_recovery() {
        let (app, user) = App::test();
        let s = &app.store;
        assert!(!s.totp_enabled(user.id).unwrap());
        // Without being on, any code passes (nothing to check).
        assert_eq!(s.totp_check(user.id, "x").unwrap(), CodeCheck::Ok);
        let secret = s.totp_begin(user.id).unwrap();
        let key = base32_decode(&secret).unwrap();
        assert!(s.totp_enable(user.id, "12345").is_err());
        let codes = s.totp_enable(user.id, &format!("{:06}", code_at(&key, now() / STEP))).unwrap();
        assert_eq!(codes.len(), RECOVERY_CODES);
        assert!(s.totp_enabled(user.id).unwrap());
        assert!(s.totp_begin(user.id).is_err());

        // The code used to enable can't be used again; the next one works once.
        assert_eq!(s.totp_check(user.id, &format!("{:06}", code_at(&key, now() / STEP))).unwrap(), CodeCheck::Wrong);
        let next = format!("{:06}", code_at(&key, now() / STEP + 1));
        assert_eq!(s.totp_check(user.id, &next).unwrap(), CodeCheck::Ok);
        assert_eq!(s.totp_check(user.id, &next).unwrap(), CodeCheck::Wrong);

        // A recovery code works once, in any case and with or without the dash.
        assert_eq!(s.totp_check(user.id, &codes[0].to_ascii_uppercase().replace('-', "")).unwrap(), CodeCheck::Ok);
        assert_eq!(s.totp_check(user.id, &codes[0]).unwrap(), CodeCheck::Wrong);
        assert_eq!(s.totp_recovery_left(user.id).unwrap(), RECOVERY_CODES - 1);

        // Too many wrong codes lock code checks for a while.
        for _ in 0..MAX_FAILURES {
            let _ = s.totp_check(user.id, "123456");
        }
        assert_eq!(s.totp_check(user.id, &codes[1]).unwrap(), CodeCheck::Locked);
        record(&secret, true);

        s.totp_disable(user.id).unwrap();
        assert!(!s.totp_enabled(user.id).unwrap());
    }

    #[test]
    fn otpauth_uri_is_escaped() {
        let u = otpauth_uri("Ondura mail.example.com", "niklas@example.com", "ABC");
        assert_eq!(u, "otpauth://totp/Ondura%20mail.example.com:niklas@example.com?secret=ABC&issuer=Ondura%20mail.example.com&algorithm=SHA1&digits=6&period=30");
    }
}
