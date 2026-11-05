//! The middleware that puts one [`crate::access`] line under every request.
//!
//! It wraps the WHOLE router rather than the `/api/` routes alone, on purpose. The question this
//! log exists to answer — "did the client call us at all?" — is not answerable from a log that
//! only records the calls that reached a route: a client pointed at the wrong path, or refused at
//! the door, is exactly the case under investigation, and it must leave a line too.

use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

use crate::access::{self, Credential};
use crate::state::AppState;

/// Log one line per request, after the status is known.
pub async fn log_requests(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    // Classified here and dropped immediately; the header value itself is never held, formatted,
    // or passed on.
    let credential = Credential::classify_request(
        request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        &state.config.auth,
        state.config.ingest.token.as_ref(),
    );
    let started = Instant::now();
    let private_response = path == "/mcp" || path.starts_with("/api/");
    let mut response = next.run(request).await;
    // One policy at the edge covers successful bodies, authentication failures, and future API
    // routes alike. This matters especially for an installed app: adding an installable manifest
    // must never grow into browser-managed offline storage of chat text or bearer credentials.
    if private_response {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    access::request(
        &method,
        &path,
        credential,
        response.status().as_u16(),
        started.elapsed().as_millis(),
    );
    response
}
