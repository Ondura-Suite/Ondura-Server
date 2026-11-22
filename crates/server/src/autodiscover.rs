//! Autodiscover: POX (`/autodiscover/autodiscover.xml`) for Outlook-style
//! and mobile clients, and the JSON variant used by newer clients.

use std::net::IpAddr;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::app::Shared;
use crate::http::basic_auth;
use crate::xml::{Node, escape};

fn base(app: &Shared) -> String {
    format!("https://{}", app.cfg.hostname)
}

fn xml_response(body: String) -> Response {
    let mut r = (StatusCode::OK, body).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/xml; charset=utf-8"));
    r
}

fn error_response(code: &str, message: &str) -> Response {
    xml_response(format!(
        r#"<?xml version="1.0" encoding="utf-8"?><Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006"><Response><Error Time="{}" Id="1"><ErrorCode>{code}</ErrorCode><Message>{}</Message><DebugData /></Error></Response></Autodiscover>"#,
        chrono::Utc::now().format("%H:%M:%S%.3f"),
        escape(message)
    ))
}

pub async fn xml(app: &Shared, headers: &HeaderMap, body: &Bytes, ip: IpAddr) -> Response {
    let user = match basic_auth(app, headers, ip).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let text = String::from_utf8_lossy(body);
    let req = Node::parse(&text).unwrap_or_default();
    let email = req.find("EMailAddress").map(|e| e.text.trim().to_ascii_lowercase()).unwrap_or_else(|| user.email.clone());
    let schema = req.find("AcceptableResponseSchema").map(|e| e.text.trim().to_string()).unwrap_or_default();
    let allowed = app.store.send_as_addresses(user.id).unwrap_or_default();
    if !allowed.iter().any(|a| a.eq_ignore_ascii_case(&email)) {
        return error_response("600", "Invalid Request");
    }
    let name = escape(if user.name.is_empty() { &user.email } else { &user.name });
    let base = base(app);
    if schema.contains("mobilesync") {
        return xml_response(format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006">
<Response xmlns="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006">
<Culture>en:us</Culture>
<User><DisplayName>{name}</DisplayName><EMailAddress>{email}</EMailAddress></User>
<Action><Settings><Server><Type>MobileSync</Type><Url>{base}/Microsoft-Server-ActiveSync</Url><Name>{base}/Microsoft-Server-ActiveSync</Name></Server></Settings></Action>
</Response>
</Autodiscover>"#,
            email = escape(&email)
        ));
    }
    let host = escape(&app.cfg.hostname);
    xml_response(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006">
<Response xmlns="http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a">
<User><DisplayName>{name}</DisplayName><AutoDiscoverSMTPAddress>{email}</AutoDiscoverSMTPAddress><DeploymentId>{deployment}</DeploymentId></User>
<Account>
<AccountType>email</AccountType>
<Action>settings</Action>
<MicrosoftOnline>False</MicrosoftOnline>
<Protocol>
<Type>EXPR</Type>
<Server>{host}</Server>
<SSL>On</SSL>
<AuthPackage>Basic</AuthPackage>
<ASUrl>{base}/EWS/Exchange.asmx</ASUrl>
<EwsUrl>{base}/EWS/Exchange.asmx</EwsUrl>
<EmwsUrl>{base}/EWS/Exchange.asmx</EmwsUrl>
<OOFUrl>{base}/EWS/Exchange.asmx</OOFUrl>
<UMUrl>{base}/EWS/Exchange.asmx</UMUrl>
<ServerExclusiveConnect>On</ServerExclusiveConnect>
</Protocol>
<Protocol>
<Type>WEB</Type>
<Internal><ASUrl>{base}/EWS/Exchange.asmx</ASUrl></Internal>
<External><ASUrl>{base}/EWS/Exchange.asmx</ASUrl></External>
</Protocol>
<Protocol>
<Type>SMTP</Type>
<Server>{host}</Server>
<Port>587</Port>
<SSL>On</SSL>
<Encryption>TLS</Encryption>
<SPA>Off</SPA>
<AuthRequired>On</AuthRequired>
<LoginName>{email}</LoginName>
</Protocol>
</Account>
</Response>
</Autodiscover>"#,
        email = escape(&email),
        deployment = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, app.cfg.hostname.as_bytes()),
    ))
}

/// `/autodiscover/autodiscover.json/v1.0/{email}?Protocol=ActiveSync|EWS`
pub fn json(app: &Shared, _path: &str, query: &str) -> Response {
    let protocol = query
        .split('&')
        .find_map(|p| p.split_once('=').filter(|(k, _)| k.eq_ignore_ascii_case("protocol")).map(|(_, v)| v.to_ascii_lowercase()))
        .unwrap_or_else(|| "activesync".into());
    let base = base(app);
    let body = match protocol.as_str() {
        "activesync" => serde_json::json!({ "Protocol": "ActiveSync", "Url": format!("{base}/Microsoft-Server-ActiveSync") }),
        "ews" => serde_json::json!({ "Protocol": "EWS", "Url": format!("{base}/EWS/Exchange.asmx") }),
        "autodiscoverv1" => serde_json::json!({ "Protocol": "AutodiscoverV1", "Url": format!("{base}/autodiscover/autodiscover.xml") }),
        other => {
            return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({ "ErrorCode": "ProtocolNotSupported", "ErrorMessage": format!("The given protocol value '{other}' is invalid.") }))).into_response();
        }
    };
    axum::Json(body).into_response()
}
