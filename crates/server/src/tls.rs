//! TLS for all listeners. The certificate is reloaded automatically when
//! the files change (Let's Encrypt renewals need no restart).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use rustls::ServerConfig;
use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tracing::{info, warn};

#[derive(Debug)]
struct Reloading {
    cert: PathBuf,
    key: PathBuf,
    current: RwLock<(Arc<CertifiedKey>, Option<SystemTime>)>,
}

impl ResolvesServerCert for Reloading {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.read().unwrap_or_else(|e| e.into_inner()).0.clone())
    }
}

fn load(cert: &Path, key: &Path) -> Result<CertifiedKey> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open(cert).with_context(|| format!("open {}", cert.display()))?,
    ))
    .collect::<std::result::Result<_, _>>()
    .with_context(|| format!("read certificates from {}", cert.display()))?;
    if certs.is_empty() {
        return Err(anyhow!("no certificate in {}", cert.display()));
    }
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut std::io::BufReader::new(
        std::fs::File::open(key).with_context(|| format!("open {}", key.display()))?,
    ))
    .with_context(|| format!("read private key from {}", key.display()))?
    .ok_or_else(|| anyhow!("no private key in {}", key.display()))?;
    let signing = any_supported_type(&key).map_err(|e| anyhow!("unsupported private key: {e}"))?;
    Ok(CertifiedKey::new(certs, signing))
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub type Reloader = Arc<dyn Fn() + Send + Sync>;

/// Server TLS configuration with automatic reloading.
pub fn server_config(cert: &Path, key: &Path, alpn: &[&[u8]]) -> Result<(Arc<ServerConfig>, Reloader)> {
    let ck = load(cert, key)?;
    let resolver = Arc::new(Reloading { cert: cert.to_path_buf(), key: key.to_path_buf(), current: RwLock::new((Arc::new(ck), mtime(cert))) });
    let mut cfg = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow!("{e}"))?
        .with_no_client_auth()
        .with_cert_resolver(resolver.clone());
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let r = resolver.clone();
    let reload: Reloader = Arc::new(move || {
        let changed = mtime(&r.cert);
        if changed == r.current.read().unwrap_or_else(|e| e.into_inner()).1 {
            return;
        }
        match load(&r.cert, &r.key) {
            Ok(ck) => {
                *r.current.write().unwrap_or_else(|e| e.into_inner()) = (Arc::new(ck), changed);
                info!(cert = %r.cert.display(), "TLS certificate reloaded");
            }
            Err(e) => warn!("TLS certificate changed but could not be loaded: {e:#}"),
        }
    });
    Ok((Arc::new(cfg), reload))
}

/// Check the certificate files every few minutes.
pub fn spawn_reloader(reloaders: Vec<Reloader>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        loop {
            tick.tick().await;
            for r in &reloaders {
                r();
            }
        }
    });
}

/// Client configuration for outgoing SMTP. Opportunistic TLS towards MX
/// hosts accepts any certificate (as all major MTAs do without MTA-STS/DANE);
/// relays are verified against the Mozilla roots.
pub fn client_config(verify: bool) -> Arc<rustls::ClientConfig> {
    let builder = rustls::ClientConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().expect("protocol versions");
    let cfg = if verify {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAny)).with_no_client_auth()
    };
    Arc::new(cfg)
}

#[derive(Debug)]
struct AcceptAny;

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &provider().signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &provider().signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        provider().signature_verification_algorithms.supported_schemes()
    }
}
