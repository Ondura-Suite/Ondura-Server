//! HTTPS front end: ActiveSync, EWS, Autodiscover and the Admin Center.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;
use tracing::{debug, info, warn};

use crate::app::Shared;
use crate::auth::{self, LoginError};
use crate::store::User;
use crate::{admin, autodiscover, eas, ews, webmail};

/// Address of the client (after trusted proxies).
#[derive(Debug, Clone, Copy)]
pub struct Peer(pub IpAddr);

/// Identifies the product and version. The Ondura Mail app uses this header to
/// recognise that it is talking to an Ondura Server rather than Exchange.
pub const SERVER_ID: &str = concat!("Ondura-Server/", env!("CARGO_PKG_VERSION"));

pub fn router(app: Shared) -> Router {
    Router::new()
        .route("/collab/ws", axum::routing::get(crate::collab::server::ws_route))
        .route("/api/v1/ws", axum::routing::get(crate::api::ws_route))
        .fallback(dispatch)
        .layer(axum::middleware::map_response(stamp_server))
        .with_state(app)
}

/// Add the Ondura identification header to every response.
async fn stamp_server(mut res: Response) -> Response {
    res.headers_mut().insert("X-Ondura-Server", HeaderValue::from_static(SERVER_ID));
    res
}

/// Unauthenticated server-identification document, so a client can confirm this
/// is an Ondura Server and learn its endpoints before signing in.
fn ondura_identity(app: &Shared) -> Response {
    let host = &app.cfg.hostname;
    // "OnduraClient" is the native API this server offers to the Ondura client;
    // other clients use EWS/ActiveSync.
    let mut protocols = vec!["OnduraClient", "EWS", "ActiveSync", "Autodiscover", "SMTP"];
    if app.collab_enabled() {
        protocols.push("Collab");
    }
    let body = serde_json::json!({
        "product": "Ondura Server",
        "vendor": "Ondura",
        "identifier": "ondura-server",
        "version": env!("CARGO_PKG_VERSION"),
        "hostname": host,
        "protocols": protocols,
        "features": {
            "webmail": app.cfg.http.webmail,
            "adminCenter": app.cfg.http.admin_center,
            "collab": app.collab_enabled(),
        },
        "endpoints": {
            "clientApi": format!("https://{host}/api/{}", crate::api::API_VERSION),
            "clientApiWebsocket": format!("wss://{host}/api/{}/ws", crate::api::API_VERSION),
            "ews": format!("https://{host}/EWS/Exchange.asmx"),
            "activesync": format!("https://{host}/Microsoft-Server-ActiveSync"),
            "autodiscover": format!("https://{host}/autodiscover/autodiscover.xml"),
            "webmail": if app.cfg.http.webmail { Some(format!("https://{host}/mail")) } else { None },
            "collab": if app.collab_enabled() { Some(format!("https://{host}/collab")) } else { None },
            "collabWebsocket": if app.collab_enabled() { Some(format!("wss://{host}/collab/ws")) } else { None },
        },
    });
    axum::Json(body).into_response()
}

fn client_ip(app: &Shared, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    if (app.cfg.http.trusted_proxies.contains(&peer) || (app.cfg.http.behind_proxy && peer.is_loopback()))
        && let Some(ip) = headers
            .get("X-Forwarded-For")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next_back())
            .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
    peer
}

pub fn unauthorized() -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, "Authentication required").into_response();
    r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Ondura Server\""));
    r
}

/// HTTP Basic authentication of a request.
/// Defence-in-depth CSRF check: when a browser sends an `Origin` on a mutating
/// request, require it to match the `Host`. Absent `Origin` (native clients,
/// same-origin GETs) passes, since those are already guarded by the custom
/// request header and the `SameSite=Strict` session cookie.
/// HTTP/2 carries the authority in the `:authority` pseudo-header instead of a
/// `Host` header. Copy it into `Host` so handlers that read `Host` (notably the
/// CSRF Origin check) behave the same on HTTP/2 and HTTP/1.1.
fn normalize_host(headers: &mut HeaderMap, uri: &axum::http::Uri) {
    if headers.contains_key(header::HOST) {
        return;
    }
    if let Some(value) = uri.authority().and_then(|a| HeaderValue::from_str(a.as_str()).ok()) {
        headers.insert(header::HOST, value);
    }
}

pub fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get("Origin").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    if origin.eq_ignore_ascii_case("null") {
        return false;
    }
    let origin_host = origin.split("://").nth(1).unwrap_or("");
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    !host.is_empty() && origin_host.eq_ignore_ascii_case(host)
}

pub async fn basic_auth(app: &Shared, headers: &HeaderMap, ip: IpAddr) -> std::result::Result<User, Box<Response>> {
    let Some((login, password)) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(auth::parse_basic) else {
        return Err(Box::new(unauthorized()));
    };
    let store = app.store.clone();
    let res = tokio::task::spawn_blocking(move || auth::login(&store, &login, &password, Some(ip))).await;
    match res {
        // EWS, ActiveSync and Autodiscover are mail/PIM protocols, so an account
        // without a mailbox cannot use them even though its credentials are valid.
        Ok(Ok(u)) if !u.mailbox => Err(Box::new(unauthorized())),
        Ok(Ok(u)) => Ok(u),
        Ok(Err(LoginError::Locked)) => Err(Box::new((StatusCode::TOO_MANY_REQUESTS, "Too many failed logins").into_response())),
        Ok(Err(LoginError::Internal(e))) => {
            warn!("login failed internally: {e:#}");
            Err(Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()))
        }
        _ => Err(Box::new(unauthorized())),
    }
}

async fn dispatch(State(app): State<Shared>, req: Request) -> Response {
    let peer = req.extensions().get::<Peer>().map(|p| p.0).unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let ip = client_ip(&app, peer, req.headers());
    let raw_path = req.uri().path().to_string();
    let path = req.uri().path().to_ascii_lowercase();
    let query = req.uri().query().unwrap_or_default().to_string();
    let method = req.method().clone();
    let mut headers = req.headers().clone();
    normalize_host(&mut headers, req.uri());
    debug!(%method, %path, %ip, "HTTP request");
    let limit = app.cfg.smtp.max_message_size * 2 + 1024 * 1024;
    let body = match axum::body::to_bytes(req.into_body(), limit).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "Request too large").into_response(),
    };
    
    match path.trim_end_matches('/') {
        "/microsoft-server-activesync" => {
            if method == axum::http::Method::OPTIONS && !headers.contains_key(header::AUTHORIZATION) {
                return unauthorized();
            }
            match basic_auth(&app, &headers, ip).await {
                Ok(user) => eas::handle(app.clone(), user, method, &query, &headers, body).await,
                Err(r) => *r,
            }
        }
        "/ews/exchange.asmx" | "/ews/services.wsdl" => match basic_auth(&app, &headers, ip).await {
            Ok(user) => ews::handle(app.clone(), user, &headers, body).await,
            Err(r) => *r,
        },
        p if p.starts_with("/autodiscover/autodiscover.json") => autodiscover::json(&app, &path, &query),
        "/autodiscover/autodiscover.xml" => autodiscover::xml(&app, &headers, &body, ip).await,
        "/.well-known/ondura-server" | "/ondura-server" => ondura_identity(&app),
        "" => {
            if app.cfg.http.webmail {
                webmail::shell()
            } else {
                Redirect::temporary("/admin/").into_response()
            }
        }
        // Each app has its own URL; all serve the same SPA shell, which reads the
        // path to decide which app to open. The API and assets stay under /mail.
        p if p == "/files" || p.starts_with("/files/") || p == "/sharepoint" || p.starts_with("/sharepoint/") || p == "/tasks" || p.starts_with("/tasks/") || p == "/notes" || p.starts_with("/notes/") => {
            if app.cfg.http.webmail && method == axum::http::Method::GET {
                webmail::shell()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
        p if p.starts_with("/admin") => {
            if !app.cfg.http.admin_center {
                return StatusCode::NOT_FOUND.into_response();
            }
            admin::handle(app.clone(), method, &path, &query, &headers, body, ip).await
        }
        p if p.starts_with("/mail") => {
            if !app.cfg.http.webmail {
                return StatusCode::NOT_FOUND.into_response();
            }
            webmail::handle(app.clone(), method, &path, &query, &headers, body, ip).await
        }
        p if p.starts_with("/collab/api") => crate::collab::server::handle(app.clone(), method, &path, &query, &headers, body).await,
        // Nextcloud-compatible surface (status.php, OCS, Login Flow v2, WebDAV).
        p if p == "/status.php" || p.starts_with("/remote.php") || p.starts_with("/ocs/") || p.starts_with("/index.php/login") => {
            crate::nextcloud::handle(app.clone(), method, &raw_path, &query, &headers, body).await
        }
        p if p == "/api" || p.starts_with("/api/") => crate::api::handle(app.clone(), method, &path, &query, &headers, body, ip).await,
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_conn<S>(router: Router, io: S, peer: SocketAddr)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let svc = tower::service_fn(move |mut req: Request<hyper::body::Incoming>| {
        let router = router.clone();
        req.extensions_mut().insert(Peer(peer.ip()));
        async move { router.oneshot(req.map(Body::new)).await }
    });
    if let Err(e) = Builder::new(TokioExecutor::new()).serve_connection_with_upgrades(TokioIo::new(io), TowerToHyperService::new(svc)).await {
        debug!(%peer, "HTTP connection: {e}");
    }
}

/// Maximum simultaneous HTTP(S) connections across all clients.
const MAX_CONNECTIONS: usize = 1024;
/// Maximum simultaneous HTTP(S) connections from a single remote IP.
const MAX_CONNECTIONS_PER_IP: usize = 64;

pub async fn serve_https(app: Shared, addr: SocketAddr) -> Result<()> {
    let Some(tls) = app.https_tls.clone() else {
        warn!(%addr, "no TLS certificate configured, HTTPS listener disabled");
        return Ok(());
    };
    let listener = TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!("bind HTTPS {addr}: {e}"))?;
    info!(%addr, "HTTPS listening");
    let acceptor = TlsAcceptor::from(tls);
    let router = router(app);
    let limiter = crate::netlimit::ConnLimiter::new(MAX_CONNECTIONS, MAX_CONNECTIONS_PER_IP);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("HTTPS accept: {e}");
                continue;
            }
        };
        let Some(guard) = limiter.admit(peer.ip()) else {
            debug!(%peer, "HTTPS connection refused: limit reached");
            continue;
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(std::time::Duration::from_secs(20), acceptor.accept(tcp)).await {
                Ok(Ok(stream)) => serve_conn(router, stream, peer).await,
                Ok(Err(e)) => debug!(%peer, "TLS handshake failed: {e}"),
                Err(_) => debug!(%peer, "TLS handshake timeout"),
            }
            drop(guard);
        });
    }
}

/// Plain HTTP: the full service behind a reverse proxy, otherwise only a
/// redirect to HTTPS.
pub async fn serve_http(app: Shared, addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!("bind HTTP {addr}: {e}"))?;
    info!(%addr, behind_proxy = app.cfg.http.behind_proxy, "HTTP listening");
    let router = if app.cfg.http.behind_proxy {
        router(app.clone())
    } else {
        let host = app.cfg.hostname.clone();
        Router::new().fallback(move |req: Request| {
            let host = host.clone();
            async move {
                let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
                Redirect::permanent(&format!("https://{host}{pq}"))
            }
        })
    };
    let _ = Arc::strong_count(&app);
    // Behind a reverse proxy every connection shares the proxy's IP, so a
    // per-IP cap would throttle all users together; fall back to the global cap.
    let per_ip = if app.cfg.http.behind_proxy { MAX_CONNECTIONS } else { MAX_CONNECTIONS_PER_IP };
    let limiter = crate::netlimit::ConnLimiter::new(MAX_CONNECTIONS, per_ip);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("HTTP accept: {e}");
                continue;
            }
        };
        let Some(guard) = limiter.admit(peer.ip()) else {
            debug!(%peer, "HTTP connection refused: limit reached");
            continue;
        };
        let router = router.clone();
        tokio::spawn(async move {
            serve_conn(router, tcp, peer).await;
            drop(guard);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    #[test]
    fn origin_check() {
        fn mk(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(axum::http::HeaderName::from_static(k), HeaderValue::from_str(v).unwrap());
            }
            h
        }
        // No Origin (native client) passes.
        assert!(origin_ok(&mk(&[("host", "mail.example.com")])));
        // Same-origin passes.
        assert!(origin_ok(&mk(&[("host", "mail.example.com"), ("origin", "https://mail.example.com")])));
        // Cross-origin is rejected.
        assert!(!origin_ok(&mk(&[("host", "mail.example.com"), ("origin", "https://evil.test")])));
        // Opaque origin is rejected.
        assert!(!origin_ok(&mk(&[("host", "mail.example.com"), ("origin", "null")])));
    }

    #[test]
    fn http2_authority_becomes_host() {
        // HTTP/2 requests have no Host header; the authority stands in for it,
        // so the Origin check sees a host and same-origin requests pass.
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::HeaderName::from_static("origin"), HeaderValue::from_static("https://mail.example.com"));
        assert!(!origin_ok(&headers)); // without a host the check can't match
        let uri: axum::http::Uri = "https://mail.example.com/mail/api/login".parse().unwrap();
        normalize_host(&mut headers, &uri);
        assert_eq!(headers.get(header::HOST).unwrap(), "mail.example.com");
        assert!(origin_ok(&headers));
        // An existing Host header (HTTP/1.1) is left untouched.
        let mut h1 = HeaderMap::new();
        h1.insert(header::HOST, HeaderValue::from_static("real.example.com"));
        normalize_host(&mut h1, &uri);
        assert_eq!(h1.get(header::HOST).unwrap(), "real.example.com");
    }

    #[tokio::test]
    async fn identifies_as_ondura() {
        let (app, _u) = crate::app::App::test();
        let res = router(app)
            .oneshot(Request::builder().uri("/.well-known/ondura-server").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get("X-Ondura-Server").unwrap(), SERVER_ID);
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["identifier"], "ondura-server");
        assert_eq!(v["product"], "Ondura Server");
    }

    #[tokio::test]
    async fn server_header_on_every_response() {
        let (app, _u) = crate::app::App::test();
        // Even an unknown path (404) carries the identification header.
        let res = router(app)
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.headers().get("X-Ondura-Server").unwrap(), SERVER_ID);
    }
}
