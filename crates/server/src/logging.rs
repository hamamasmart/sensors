//! Request logging middleware.
//!
//! Logs one line per request — method, path, status code, and latency — via
//! `tracing` so it flows through the same `EnvFilter` subscriber as the rest of
//! the server. Applied to the whole router so the liveness probe is logged too.

use axum::{body::Body, http::Request, middleware::Next, response::Response};

pub async fn request_log(request: Request<Body>, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let start = std::time::Instant::now();

    let response = next.run(request).await;

    let status = response.status();
    let elapsed = start.elapsed();

    // `WARN` for server errors, `INFO` otherwise — the level/parent span is set
    // by the `EnvFilter` subscriber in `main`.
    if status.is_server_error() || status.is_client_error() {
        tracing::warn!(%method, %path, %status, elapsed = ?elapsed, "request failed");
    } else {
        tracing::info!(%method, %path, %status, elapsed = ?elapsed, "request");
    }

    response
}
