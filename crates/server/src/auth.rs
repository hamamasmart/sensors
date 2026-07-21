//! Bearer-token auth gate applied to every route.
//!
//! Callers must send `Authorization: Bearer <AUTH_TOKEN>`. A missing or
//! mismatched header short-circuits with `401 Unauthorized` before the handler
//! runs.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header},
    middleware::Next,
    response::Response,
};

/// Axum middleware state: the single expected bearer token.
#[derive(Clone)]
pub struct ExpectedToken(pub String);

/// Reject any request whose `Authorization` header is not `Bearer <token>`.
pub async fn require_bearer_token(
    State(ExpectedToken(expected)): State<ExpectedToken>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer ").map(str::trim));

    match provided {
        Some(token) if token == expected => Ok(next.run(request).await),
        // No token, wrong scheme, or mismatch — treat all the same to avoid
        // leaking which check failed.
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}
