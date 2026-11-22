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
use crate::{admin, autodiscover, eas, ews};

/// Address of the client (after trusted proxies).
#[derive(Debug, Clone, Copy)]
pub struct Peer(pub IpAddr);

pub fn router(app: Shared) -> Router {
    Router::new().fallback(dispatch).with_state(app)
}

fn client_ip(app: &Shared, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    if app.cfg.http.trusted_proxies.contains(&peer) || (app.cfg.http.behind_proxy && peer.is_loopback()) {
        if let Some(ip) = headers
            .get("X-Forwarded-For")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next_back())
            .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
    }
    peer
}

pub fn unauthorized() -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, "Authentication required").into_response();
    r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Ondura Server\""));
    r
}

/// HTTP Basic authentication of a request.
pub async fn basic_auth(app: &Shared, headers: &HeaderMap, ip: IpAddr) -> std::result::Result<User, Response> {
    let Some((login, password)) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(auth::parse_basic) else {
        return Err(unauthorized());
    };
    let store = app.store.clone();
    let res = tokio::task::spawn_blocking(move || auth::login(&store, &login, &password, Some(ip))).await;
    match res {
        Ok(Ok(u)) => Ok(u),
        Ok(Err(LoginError::Locked)) => Err((StatusCode::TOO_MANY_REQUESTS, "Too many failed logins").into_response()),
        Ok(Err(LoginError::Internal(e))) => {
            warn!("login failed internally: {e:#}");
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        _ => Err(unauthorized()),
    }
}

async fn dispatch(State(app): State<Shared>, req: Request) -> Response {
    let peer = req.extensions().get::<Peer>().map(|p| p.0).unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let ip = client_ip(&app, peer, req.headers());
    let path = req.uri().path().to_ascii_lowercase();
    let query = req.uri().query().unwrap_or_default().to_string();
    let method = req.method().clone();
    let headers = req.headers().clone();
    debug!(%method, %path, %ip, "HTTP request");
    let limit = app.cfg.smtp.max_message_size * 2 + 1024 * 1024;
    let body = match axum::body::to_bytes(req.into_body(), limit).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "Request too large").into_response(),
    };
    let response = match path.trim_end_matches('/') {
        "/microsoft-server-activesync" => {
            if method == axum::http::Method::OPTIONS && !headers.contains_key(header::AUTHORIZATION) {
                return unauthorized();
            }
            match basic_auth(&app, &headers, ip).await {
                Ok(user) => eas::handle(app.clone(), user, method, &query, &headers, body).await,
                Err(r) => r,
            }
        }
        "/ews/exchange.asmx" | "/ews/services.wsdl" => match basic_auth(&app, &headers, ip).await {
            Ok(user) => ews::handle(app.clone(), user, &headers, body).await,
            Err(r) => r,
        },
        p if p.starts_with("/autodiscover/autodiscover.json") => autodiscover::json(&app, &path, &query),
        "/autodiscover/autodiscover.xml" => autodiscover::xml(&app, &headers, &body, ip).await,
        "" => Redirect::temporary("/admin/").into_response(),
        p if p.starts_with("/admin") => {
            if !app.cfg.http.admin_center {
                return StatusCode::NOT_FOUND.into_response();
            }
            admin::handle(app.clone(), method, &path, &query, &headers, body, ip).await
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    };
    response
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

pub async fn serve_https(app: Shared, addr: SocketAddr) -> Result<()> {
    let Some(tls) = app.https_tls.clone() else {
        warn!(%addr, "no TLS certificate configured, HTTPS listener disabled");
        return Ok(());
    };
    let listener = TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!("bind HTTPS {addr}: {e}"))?;
    info!(%addr, "HTTPS listening");
    let acceptor = TlsAcceptor::from(tls);
    let router = router(app);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("HTTPS accept: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(std::time::Duration::from_secs(20), acceptor.accept(tcp)).await {
                Ok(Ok(stream)) => serve_conn(router, stream, peer).await,
                Ok(Err(e)) => debug!(%peer, "TLS handshake failed: {e}"),
                Err(_) => debug!(%peer, "TLS handshake timeout"),
            }
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
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("HTTP accept: {e}");
                continue;
            }
        };
        let router = router.clone();
        tokio::spawn(serve_conn(router, tcp, peer));
    }
}
