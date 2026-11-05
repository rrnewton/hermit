//! The Streamable HTTP endpoint at `/mcp`.
//!
//! # Why Streamable HTTP and not HTTP+SSE
//!
//! MCP's original remote transport was HTTP+SSE: a long-lived `GET /sse` stream plus a separate
//! `POST /messages` endpoint correlated by session. It was superseded by Streamable HTTP, which
//! is a single endpoint that answers a POST either with a plain JSON body or with an SSE stream,
//! and it is the transport with a future. This server implements Streamable HTTP only. HTTP+SSE
//! is deliberately NOT offered, rather than offered badly: the legacy transport requires
//! server-held sessions and an always-open stream per client, which is real state on a
//! publicly-reachable process, and it is being removed across the ecosystem anyway.
//!
//! # Stateless by construction
//!
//! No session is issued and none is required: every POST carries its own credential and is
//! answered on the spot. That means no server-side session table to leak, expire, or fixate, and
//! it means a restart cannot strand a client mid-conversation. The cost is that server-initiated
//! messages are impossible — this server has none to send — so `GET /mcp`, which exists in the
//! spec to open that channel, honestly answers 405 instead of holding a stream open that would
//! never carry anything.
//!
//! # The order of checks
//!
//! Authenticate, then parse, then authorize the specific call. An unauthenticated caller is
//! answered `401` with a fixed body before the request is even read as JSON, so no error message
//! can be used to learn which tools, channels, or protocol revisions this deployment has.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::auth::{self, Scope};
use crate::mcp::protocol::{self, Outcome, RpcError, RpcResponse, INVALID_REQUEST};
use crate::state::AppState;

/// The path this endpoint is mounted at, named once so documentation and tests agree.
pub const MCP_PATH: &str = "/mcp";

/// Header a client uses to state which protocol revision it settled on.
const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// A bland 401. It says nothing about what a correct credential would look like, which tools
/// exist, or whether this deployment is configured at all.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, "Bearer"),
            (header::CONTENT_TYPE, "application/json"),
        ],
        r#"{"error":"unauthorized"}"#,
    )
        .into_response()
}

/// Serialize a JSON-RPC response as either a plain JSON body or a one-event SSE stream, according
/// to what the client said it accepts.
///
/// Streamable HTTP lets the server choose. A single request/response pair has nothing to stream,
/// so JSON is the honest answer — but clients that only accept `text/event-stream` exist, and
/// they are met on their own terms rather than failed.
fn encode(headers: &HeaderMap, status: StatusCode, response: &RpcResponse) -> Response {
    let body = serde_json::to_string(response)
        .unwrap_or_else(|_| r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"could not serialize response"}}"#.to_owned());
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let wants_json =
        accept.is_empty() || accept.contains("application/json") || accept.contains("*/*");
    if wants_json || !accept.contains("text/event-stream") {
        return (status, [(header::CONTENT_TYPE, "application/json")], body).into_response();
    }
    // One event, then the stream ends: there is nothing further to say.
    let sse = format!("event: message\ndata: {body}\n\n");
    (
        status,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        sse,
    )
        .into_response()
}

/// `POST /mcp` — the whole MCP surface.
pub async fn post(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let Some(scope) = auth::scope_of(header, &state.config.auth) else {
        return unauthorized();
    };

    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        let response = RpcResponse {
            jsonrpc: "2.0",
            id: Value::Null,
            result: None,
            error: Some(RpcError::new(INVALID_REQUEST, "body is not valid JSON")),
        };
        return encode(&headers, StatusCode::BAD_REQUEST, &response);
    };

    match protocol::dispatch(&state, scope, &value).await {
        Outcome::Reply(response) => encode(&headers, StatusCode::OK, &response),
        // A notification has no response body at all; 202 is what the spec asks for.
        Outcome::Accepted => StatusCode::ACCEPTED.into_response(),
        // The JSON-RPC error is carried in the body so an MCP client can render it, and the HTTP
        // status is 403 so anything in front of this server — a tunnel access log, a proxy — can
        // see that a credential was refused without parsing JSON-RPC.
        Outcome::Forbidden(response) => encode(&headers, StatusCode::FORBIDDEN, &response),
    }
}

/// `GET /mcp` and `DELETE /mcp`.
///
/// The spec permits a server that has no server-initiated messages and no sessions to refuse
/// both. Saying 405 plainly is better than an open stream that never emits or a session-delete
/// that pretends to have deleted something.
pub async fn method_not_allowed(headers: HeaderMap) -> Response {
    let _ = headers.get(PROTOCOL_VERSION_HEADER);
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [
            (header::ALLOW, "POST"),
            (header::CONTENT_TYPE, "application/json"),
        ],
        json!({
            "error": "method_not_allowed",
            "detail": "this endpoint is stateless Streamable HTTP: POST only",
        })
        .to_string(),
    )
        .into_response()
}

/// Whether a credential with this scope may call a mutating tool. Exposed for documentation
/// symmetry with [`crate::auth::authorize`]; the enforcement itself is in
/// [`crate::mcp::protocol::dispatch`].
#[must_use]
pub fn may_post(scope: Scope) -> bool {
    scope >= Scope::Write
}
