//! The door every bearer-token `/api/` request passes before it is routed.
//!
//! `#42 api-scope-first`. The handler extractors ([`super::api::ReadScope`],
//! [`super::api::WriteScope`]) keep the parsers from answering a caller without a credential, but
//! routing answers before any handler runs: an unknown path would get 404 and an unserved method
//! 405 with an `allow` header, so an anonymous caller could map the API while a real route said
//! 401. This layer answers that caller 401 first, with the same status, headers and body a route
//! gives, so "every `/api/` route requires a bearer token" is true of every answer under `/api/`.
//!
//! It must wrap the router from OUTSIDE (see [`super::router`]): `Router::layer` runs after
//! routing has picked an endpoint, so the 405 path would still add its `allow` header to this 401.
//!
//! It checks only that ONE of the two browser tokens was presented. Which scope a route needs stays
//! with the route, so a read token still gets the route's 403, and a caller with a valid token
//! still gets 404 and 405 from routing exactly as before.

use axum::extract::{Request, State};
use axum::http::{header, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::{self, AuthError};
use crate::http::api::ApiError;
use crate::state::AppState;

/// Refuse a request with no valid browser token before routing, unless its route has its own.
pub async fn require_bearer(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if needs_bearer(request.method(), request.uri().path()) {
        let header = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if auth::scope_of(header, &state.config.auth).is_none() {
            return ApiError::from(AuthError::Unauthenticated).into_response();
        }
    }
    next.run(request).await
}

/// Whether a request must carry one of the two browser tokens before it is routed.
///
/// Everything under `/api/` does, except the four requests whose route carries a different
/// credential and checks it before reading anything else: the adapter's ingest token, and the
/// read-aloud ticket in the path that stands in for the header an `<audio src>` cannot send.
fn needs_bearer(method: &Method, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/") else {
        return false;
    };
    let mut segments = rest.split('/');
    let segments = [
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ];
    let own_credential = match segments {
        [Some("v1"), Some("live"), Some("events" | "hints"), None, _] => method == Method::POST,
        // `HEAD` because axum answers it wherever it answers `GET`, and a media element may use it.
        // An empty ticket is never a live one, so it earns no exemption and routing never answers it.
        [Some("v1"), Some("speech"), Some(ticket), None, _] if !ticket.is_empty() => {
            method == Method::GET || method == Method::HEAD
        }
        [Some("v1"), Some("speech"), Some(ticket), Some("timing"), None] if !ticket.is_empty() => {
            method == Method::POST
        }
        _ => false,
    };
    !own_credential
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_api_paths_need_a_bearer_token() {
        for path in ["/", "/healthz", "/voice", "/mcp", "/api", "/apiv1/channels"] {
            assert!(!needs_bearer(&Method::GET, path), "{path}");
        }
        for path in [
            "/api/",
            "/api/v1/channels",
            "/api/v1/nope",
            "/api/v2/channels",
        ] {
            assert!(needs_bearer(&Method::GET, path), "{path}");
        }
    }

    #[test]
    fn the_routes_with_their_own_credential_are_exempt_only_for_their_own_method() {
        assert!(!needs_bearer(&Method::POST, "/api/v1/live/events"));
        assert!(needs_bearer(&Method::GET, "/api/v1/live/events"));
        assert!(!needs_bearer(&Method::POST, "/api/v1/live/hints"));
        assert!(needs_bearer(&Method::GET, "/api/v1/live/hints"));
        assert!(!needs_bearer(&Method::GET, "/api/v1/speech/ticket"));
        assert!(!needs_bearer(&Method::HEAD, "/api/v1/speech/ticket"));
        assert!(needs_bearer(&Method::POST, "/api/v1/speech/ticket"));
        assert!(!needs_bearer(&Method::POST, "/api/v1/speech/ticket/timing"));
        assert!(needs_bearer(&Method::GET, "/api/v1/speech/ticket/timing"));
        assert!(needs_bearer(&Method::GET, "/api/v1/speech/ticket/other"));
        assert!(needs_bearer(&Method::GET, "/api/v1/speech/ticket/"));
        assert!(needs_bearer(&Method::GET, "/api/v1/speech/"));
        assert!(needs_bearer(&Method::POST, "/api/v1/speech//timing"));
    }
}
