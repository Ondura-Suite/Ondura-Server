//! DKIM keys per domain and signing of outgoing mail.

use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use mail_auth::common::crypto::{RsaKey, Sha256};
use mail_auth::common::headers::HeaderWriter;
use mail_auth::dkim::DkimSigner;
use mail_auth::dkim::generate::DkimKeyPair;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs1KeyDer};

use crate::store::{DkimKey, Store};

/// Create (or replace) the DKIM key of a domain.
pub fn generate(store: &Store, domain: &str) -> Result<DkimKey> {
    let pair = DkimKeyPair::generate_rsa(2048).map_err(|e| anyhow!("DKIM key generation failed: {e}"))?;
    let key = DkimKey {
        domain: domain.to_ascii_lowercase(),
        selector: format!("ondura{}", chrono::Utc::now().format("%Y%m")),
        private_key: B64.encode(pair.private_key()),
        public_key: B64.encode(pair.public_key()),
    };
    store.set_dkim(&key)?;
    Ok(key)
}

/// Value of the TXT record `<selector>._domainkey.<domain>`.
pub fn dns_record(key: &DkimKey) -> String {
    format!("v=DKIM1; k=rsa; p={}", key.public_key)
}

const SIGNED_HEADERS: &[&str] = &["From", "To", "Cc", "Subject", "Date", "Message-ID", "Reply-To", "In-Reply-To", "References", "MIME-Version", "Content-Type"];

/// DKIM-Signature header line (with CRLF) for a message, if the domain has a key.
pub fn sign(store: &Store, domain: &str, raw: &[u8]) -> Result<Option<String>> {
    let Some(key) = store.dkim(domain)? else { return Ok(None) };
    let der = B64.decode(&key.private_key)?;
    let rsa = RsaKey::<Sha256>::from_key_der(PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(der))).map_err(|e| anyhow!("DKIM key: {e}"))?;
    let signature = DkimSigner::from_key(rsa)
        .domain(key.domain.clone())
        .selector(key.selector.clone())
        .headers(SIGNED_HEADERS.iter().copied())
        .sign(raw)
        .map_err(|e| anyhow!("DKIM signing failed: {e}"))?;
    Ok(Some(signature.to_header()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_messages() {
        let (s, _) = crate::store::test_store();
        assert!(sign(&s, "example.com", b"From: a@example.com\r\n\r\nx").unwrap().is_none());
        let key = generate(&s, "example.com").unwrap();
        assert!(dns_record(&key).starts_with("v=DKIM1; k=rsa; p=MII"));
        let h = sign(&s, "example.com", b"From: a@example.com\r\nSubject: Test\r\n\r\nHallo\r\n").unwrap().unwrap();
        assert!(h.starts_with("DKIM-Signature: v=1; a=rsa-sha256;"));
        assert!(h.contains("d=example.com"));
    }
}
