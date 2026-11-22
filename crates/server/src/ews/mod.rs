use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::app::Shared;
use crate::store::User;

pub async fn handle(_app: Shared, _user: User, _headers: &HeaderMap, _body: Bytes) -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}
