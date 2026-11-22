use std::net::IpAddr;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::app::Shared;

pub async fn handle(_app: Shared, _method: Method, _path: &str, _query: &str, _headers: &HeaderMap, _body: Bytes, _ip: IpAddr) -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}
