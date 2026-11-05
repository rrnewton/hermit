//! `#42 api-scope-first`: every `/api/` route decides who is asking before it reads the request.
//!
//! Axum runs extractors in argument order, and `Path`, `Query`, `Json` and `Bytes` each refuse a
//! malformed request with their own 400, 413, 415 or 422. A route that checked its credential only
//! inside the handler therefore answered a caller with no credential — or the wrong one — with the
//! parser's opinion of its request instead of 401 or 403, which is a free description of what the
//! route accepts. So this file sends every route a well-formed request and a set of malformed ones
//! from each caller that may not use it, and requires the answers to be identical.
//!
//! The route list is an INVENTORY, checked against the router's source: a route added to
//! `src/http/mod.rs` without an entry here fails the first test, so a new route cannot opt out.
//! The same inventory is how `#223 asset-caching` pins that no `/api/` answer is ever cacheable.

use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;
use vibe_talk::http::router;
use vibe_talk::speech_tickets::{Prepared, SpeechTickets};
use vibe_talk::testing::{READ_CHANNEL, READ_TOKEN, WRITE_TOKEN};

const INGEST_TOKEN: &str = "test-ingest-token-0000000000";
/// A bearer token that is neither of the configured ones.
const WRONG_TOKEN: &str = "not-a-configured-token-00000";
/// A path segment that does not percent-decode to UTF-8, so `Path` refuses it with 400.
const UNDECODABLE: &str = "%FF";

/// Who may call a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Auth {
    /// The static web app and `/healthz`: no credential, by design.
    Public,
    /// The read or the write bearer token.
    Read,
    /// The write bearer token only.
    Write,
    /// The random ticket in the path, because an `<audio src>` cannot send a header.
    Ticket,
    /// The separate live-ingestion token.
    Ingest,
}

/// What a route reads from the request beyond its path.
#[derive(Clone, Copy, Debug)]
enum Input {
    Nothing,
    /// A query string. The first is well formed, the second fails to deserialize.
    Query(&'static str, &'static str),
    /// A JSON body, well formed, read by `Json`.
    Json(&'static str),
    /// A JSON body, well formed, that the handler reads itself and refuses with 400 when it is not
    /// the right shape.
    OwnJson(&'static str),
    /// A raw body with a size limit, well formed.
    Bytes(&'static str, usize),
}

struct Route {
    method: &'static str,
    path: &'static str,
    auth: Auth,
    input: Input,
}

const fn route(method: &'static str, path: &'static str, auth: Auth, input: Input) -> Route {
    Route {
        method,
        path,
        auth,
        input,
    }
}

/// Axum's default request-body limit, which applies to `Bytes` when a route sets none.
const DEFAULT_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Every route the router serves, and who may call it.
fn inventory() -> Vec<Route> {
    use Auth::{Ingest, Public, Read, Ticket, Write};
    use Input::{Bytes, Json, Nothing, OwnJson, Query};
    let limit = Query("?limit=5", "?limit=soon");
    vec![
        route("GET", "/healthz", Public, Nothing),
        route("GET", "/", Public, Nothing),
        route("GET", "/app.js", Public, Nothing),
        route("GET", "/voice", Public, Nothing),
        route("GET", "/voice.js", Public, Nothing),
        route("GET", "/contract.js", Public, Nothing),
        route("GET", "/voice.css", Public, Nothing),
        route("GET", "/style.css", Public, Nothing),
        route("GET", "/manifest.webmanifest", Public, Nothing),
        route("GET", "/icons/{name}", Public, Nothing),
        route("GET", "/api/v1/channels", Read, Nothing),
        route(
            "GET",
            "/api/v1/activity",
            Read,
            Query("?hours=5", "?hours=soon"),
        ),
        route("GET", "/api/v1/agent-tools", Read, Nothing),
        route("GET", "/api/v1/client-config", Read, Nothing),
        route(
            "POST",
            "/api/v1/live/events",
            Ingest,
            OwnJson(r#"{"kind":"typing"}"#),
        ),
        route(
            "POST",
            "/api/v1/live/hints",
            Ingest,
            OwnJson(r#"{"channel_id":"3333333333333333333"}"#),
        ),
        route("GET", "/api/v1/diagnostics", Read, Nothing),
        route("GET", "/api/v1/signed-url", Write, Nothing),
        route("GET", "/api/v1/voice-session", Write, Nothing),
        route("GET", "/api/v1/voice-agent", Read, Nothing),
        route(
            "POST",
            "/api/v1/voice-timing",
            Write,
            Bytes("{}", DEFAULT_BODY_LIMIT),
        ),
        route(
            "POST",
            "/api/v1/voice-health",
            Write,
            Bytes("{}", vibe_talk::voice_health::MAX_BODY_BYTES),
        ),
        route(
            "POST",
            "/api/v1/voice-end",
            Write,
            Bytes("{}", vibe_talk::voice_health::MAX_BODY_BYTES),
        ),
        route("GET", "/api/v1/channels/{channel_id}/messages", Read, limit),
        route(
            "GET",
            "/api/v1/channels/{channel_id}/messages/{message_id}",
            Read,
            limit,
        ),
        route(
            "GET",
            "/api/v1/channels/{channel_id}/messages/{message_id}/summary",
            Read,
            limit,
        ),
        route("GET", "/api/v1/channels/{channel_id}/page", Read, limit),
        route("GET", "/api/v1/channels/{channel_id}/timeline", Read, limit),
        route(
            "GET",
            "/api/v1/channels/{channel_id}/count",
            Read,
            Query("?cap=5", "?cap=soon"),
        ),
        route("GET", "/api/v1/channels/{channel_id}/digest", Read, limit),
        route(
            "POST",
            "/api/v1/channels/{channel_id}/resolve",
            Read,
            Json(r#"{"query":"lunch"}"#),
        ),
        route(
            "POST",
            "/api/v1/channels/{channel_id}/reply",
            Write,
            Json(r#"{"text":"hi"}"#),
        ),
        route(
            "POST",
            "/api/v1/channels/{channel_id}/ask",
            Write,
            Json(r#"{"question":"why?"}"#),
        ),
        route(
            "GET",
            "/api/v1/post-proposals",
            Write,
            Query("?wait=0", "?wait=soon"),
        ),
        route(
            "POST",
            "/api/v1/post-proposals",
            Write,
            Json(r#"{"channel_id":"1111111111","text":"hi"}"#),
        ),
        route(
            "POST",
            "/api/v1/post-proposals/commit",
            Write,
            Json(
                r#"{"handle":"x","channel_id":"1111111111","text":"hi","confirmed_by":"owner_ui"}"#,
            ),
        ),
        route(
            "POST",
            "/api/v1/post-proposals/cancel",
            Write,
            Json(r#"{"handle":"x"}"#),
        ),
        route("GET", "/api/v1/channels/{channel_id}/stream", Read, Nothing),
        route("GET", "/api/v1/conversations", Write, Nothing),
        route("DELETE", "/api/v1/conversations", Write, Nothing),
        route(
            "GET",
            "/api/v1/conversations/{conversation_id}",
            Write,
            Nothing,
        ),
        route(
            "DELETE",
            "/api/v1/conversations/{conversation_id}",
            Write,
            Nothing,
        ),
        route(
            "POST",
            "/api/v1/conversations/{conversation_id}/turns",
            Write,
            Json(r#"{"speaker":"user","text":"hi"}"#),
        ),
        route(
            "GET",
            "/api/v1/conversations/{conversation_id}/replay",
            Write,
            Nothing,
        ),
        route("GET", "/api/v1/transcript", Write, limit),
        route("DELETE", "/api/v1/storage", Write, Nothing),
        route("GET", "/api/v1/inbox", Read, Nothing),
        route(
            "POST",
            "/api/v1/channels/{channel_id}/messages/{message_id}/speak",
            Read,
            limit,
        ),
        route(
            "POST",
            "/api/v1/channels/{channel_id}/speech/prepare",
            Read,
            Json(r#"{"ids":["3333333333"]}"#),
        ),
        route(
            "GET",
            "/api/v1/speech/{ticket}",
            Ticket,
            Query("?selection=a", "?selection=a&selection=b"),
        ),
        route(
            "POST",
            "/api/v1/speech/{ticket}/timing",
            Ticket,
            Json(
                r#"{"tap_to_play_ms":1,"request_to_loaded_ms":1,"loaded_to_playing_ms":1,"tap_to_audible_ms":1}"#,
            ),
        ),
        route(
            "POST",
            "/api/v1/channels",
            Write,
            Json(r#"{"channel_id":"4444444444"}"#),
        ),
        route(
            "GET",
            "/api/v1/channel-directory",
            Write,
            Query("?limit=5", "?limit=soon"),
        ),
        route("DELETE", "/api/v1/channels/{channel_id}", Write, Nothing),
        // `#199 removable-config-channels`: putting a configured channel back on the list.
        route(
            "DELETE",
            "/api/v1/channels/{channel_id}/hidden",
            Write,
            Nothing,
        ),
        route("GET", "/api/v1/channels/{channel_id}/todo", Read, limit),
        route(
            "POST",
            "/api/v1/channels/{channel_id}/dismiss",
            Write,
            Json(r#"{"messages":["3333333333"]}"#),
        ),
        route(
            "POST",
            "/api/v1/channels/{channel_id}/restore",
            Write,
            Json(r#"{"messages":["3333333333"]}"#),
        ),
        route(
            "POST",
            "/api/v1/channels/{channel_id}/read",
            Write,
            Json(r#"{"message_id":"3333333333"}"#),
        ),
        route(
            "DELETE",
            "/api/v1/channels/{channel_id}/read",
            Write,
            Nothing,
        ),
        route(
            "POST",
            "/api/v1/channels/{channel_id}/upstream-read",
            Write,
            Json(r#"{"message_id":"3333333333"}"#),
        ),
        // `#232 upstream-mark-read`: seen at read scope, changed at write.
        route(
            "GET",
            "/api/v1/channels/{channel_id}/upstream-clear",
            Read,
            Nothing,
        ),
        route(
            "PUT",
            "/api/v1/channels/{channel_id}/upstream-clear",
            Write,
            Json(r#"{"mode":"off"}"#),
        ),
        // `#253 upstream-clear-polish`: "Mark read now" pushes a message, so write.
        route(
            "POST",
            "/api/v1/channels/{channel_id}/upstream-clear/now",
            Write,
            Nothing,
        ),
        route(
            "PUT",
            "/api/v1/channels/{channel_id}/alias",
            Write,
            Json(r#"{"alias":"lunch"}"#),
        ),
        route(
            "DELETE",
            "/api/v1/channels/{channel_id}/alias",
            Write,
            Nothing,
        ),
        route(
            "PUT",
            "/api/v1/noise-rules",
            Write,
            Json(r#"{"rules":["Working…"]}"#),
        ),
        // `#264 voice-send-approval`.
        route("GET", "/api/v1/voice-settings", Read, Nothing),
        route(
            "PUT",
            "/api/v1/voice-settings",
            Write,
            Json(r#"{"ask_before_send":true}"#),
        ),
        // `#272 voice-agent-instructions`.
        route("GET", "/api/v1/voice-prompt", Read, Nothing),
        route(
            "PUT",
            "/api/v1/voice-prompt",
            Write,
            Json(r#"{"instructions":"Be brief."}"#),
        ),
        route(
            "POST",
            "/api/v1/channels/{channel_id}/not-noise",
            Write,
            Json(r#"{"messages":["3333333333"]}"#),
        ),
        // `#206 pin-message`: the owner's own pins, listed at read scope and changed at write.
        route("GET", "/api/v1/channels/{channel_id}/pins", Read, Nothing),
        route(
            "PUT",
            "/api/v1/channels/{channel_id}/pins/{message_id}",
            Write,
            Json(
                r#"{"author":"a","author_id":"7","content":"hi","timestamp":"2026-10-04T12:00:00Z"}"#,
            ),
        ),
        route(
            "DELETE",
            "/api/v1/channels/{channel_id}/pins/{message_id}",
            Write,
            Nothing,
        ),
        // `#240 reading-bookmark`: the owner's place in a channel, seen at read scope and moved or
        // cleared at write.
        route(
            "GET",
            "/api/v1/channels/{channel_id}/bookmark",
            Read,
            Nothing,
        ),
        route(
            "PUT",
            "/api/v1/channels/{channel_id}/bookmark",
            Write,
            Json(r#"{"message_id":"3333333333","timestamp":"2026-10-12T12:00:00Z"}"#),
        ),
        route(
            "DELETE",
            "/api/v1/channels/{channel_id}/bookmark",
            Write,
            Query("?if_revision=5", "?if_revision=soon"),
        ),
    ]
}

/// Every `(METHOD, path)` the router source registers with a literal path.
///
/// A deliberately small reading of `src/http/mod.rs`: each `.route(` call's first argument, and
/// every method router inside its argument list, in `fn routes`. The one thing allowed outside it
/// is the outer router that wraps `routes` to carry the middlewares. It fails closed rather than
/// guessing: a way of registering routes it does not read is refused outright, and `any(` or `on(` is recorded as the
/// method `ANY` or `ON`, which no inventory entry matches. The one route registered through a
/// constant, `/mcp`, is outside `/api/` and has its own credential tests in `tests/mcp.rs`.
fn routes_in_router_source() -> BTreeSet<(String, String)> {
    let whole = include_str!("../src/http/mod.rs");
    let (outer, source) = whole
        .split_once("\nfn routes(")
        .expect("src/http/mod.rs has a `fn routes`");
    assert!(
        outer.matches(".fallback_service(").count() == 1
            && outer.contains(".fallback_service(routes(state.clone()))")
            && !outer.contains(".route("),
        "outside `fn routes`, src/http/mod.rs may only wrap `routes` in the outer router"
    );
    for unread in [
        ".route_service(",
        ".nest(",
        ".nest_service(",
        ".merge(",
        ".fallback_service(",
    ] {
        assert!(
            !source.contains(unread) && (unread == ".fallback_service(" || !outer.contains(unread)),
            "src/http/mod.rs registers routes with `{unread}`, which this inventory cannot read; \
             teach routes_in_router_source to read it before using it"
        );
    }
    let mut found = BTreeSet::new();
    let mut constants = 0;
    let mut rest = source;
    while let Some(at) = rest.find(".route(") {
        rest = &rest[at + ".route(".len()..];
        let mut depth = 1;
        let end = rest
            .char_indices()
            .find_map(|(index, c)| {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(index);
                        }
                    }
                    _ => {}
                }
                None
            })
            .expect("every .route( call closes");
        let arguments = &rest[..end];
        let Some(literal) = arguments.trim_start().strip_prefix('"') else {
            assert!(
                arguments
                    .trim_start()
                    .starts_with("crate::mcp::transport::MCP_PATH"),
                "a route whose path is neither a literal nor MCP_PATH: {arguments}"
            );
            constants += 1;
            continue;
        };
        let path = &literal[..literal.find('"').expect("a closed literal")];
        for method in [
            "get", "post", "put", "delete", "patch", "head", "options", "trace", "connect", "any",
            "on",
        ] {
            let needle = format!("{method}(");
            let mut from = 0;
            while let Some(offset) = arguments[from..].find(&needle) {
                let index = from + offset;
                let before = arguments[..index].chars().next_back();
                if !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                    found.insert((method.to_ascii_uppercase(), path.to_owned()));
                }
                from = index + needle.len();
            }
        }
    }
    assert_eq!(
        constants, 1,
        "exactly one route, /mcp, is registered by constant"
    );
    found
}

#[test]
fn the_inventory_is_every_route_the_router_serves() {
    let listed: BTreeSet<(String, String)> = inventory()
        .iter()
        .map(|route| (route.method.to_owned(), route.path.to_owned()))
        .collect();
    assert_eq!(inventory().len(), listed.len(), "a route is listed twice");
    let served = routes_in_router_source();
    let unlisted: Vec<_> = served.difference(&listed).collect();
    let stale: Vec<_> = listed.difference(&served).collect();
    assert!(
        unlisted.is_empty() && stale.is_empty(),
        "routes served but not in this inventory: {unlisted:?}; listed but not served: {stale:?}"
    );
    // The only routes without a bearer token are the ones the router's policy names.
    for route in inventory() {
        let bearer = matches!(route.auth, Auth::Read | Auth::Write);
        if route.path.starts_with("/api/") && !bearer {
            assert!(
                matches!(route.auth, Auth::Ticket | Auth::Ingest),
                "{} {} is under /api/ without a credential",
                route.method,
                route.path
            );
        }
    }
}

struct Harness {
    router: axum::Router,
    tickets: std::sync::Arc<SpeechTickets>,
}

fn harness() -> Harness {
    let text = format!(
        "{}\n[ingest]\ntoken = \"{INGEST_TOKEN}\"\n",
        vibe_talk::testing::config_toml()
    );
    let (state, _chat, _voice) = vibe_talk::testing::state_from_toml(&text);
    let tickets = std::sync::Arc::clone(&state.speech_tickets);
    Harness {
        router: router(state),
        tickets,
    }
}

/// One request, and what the router answered: status and JSON body (`Null` when not JSON).
#[derive(Clone, Debug)]
struct Sent {
    uri: String,
    token: Option<&'static str>,
    content_type: Option<&'static str>,
    body: String,
    /// The status an authorized caller gets for this request, when it is malformed.
    parsed: Option<StatusCode>,
}

async fn answer(harness: &Harness, method: &str, sent: &Sent) -> (StatusCode, Value) {
    let (status, _, body) = respond(harness, method, sent).await;
    (status, body)
}

/// As [`answer`], with the response headers too, sorted, except the two that are not the
/// router's to choose: `content-length`, which the connection sets, and `date`.
async fn respond(
    harness: &Harness,
    method: &str,
    sent: &Sent,
) -> (StatusCode, Vec<(String, String)>, Value) {
    let mut builder = Request::builder().method(method).uri(&sent.uri);
    if let Some(token) = sent.token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(content_type) = sent.content_type {
        builder = builder.header("content-type", content_type);
    }
    let request = builder
        .body(Body::from(sent.body.clone()))
        .expect("request");
    let response = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    let status = response.status();
    let mut headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter(|(name, _)| *name != "content-length" && *name != "date")
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or("<opaque>").to_owned(),
            )
        })
        .collect();
    headers.sort();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The route's path with every parameter filled in well, except `broken`, which is undecodable.
fn fill(path: &str, ticket: &str, broken: Option<&str>) -> String {
    let mut filled = path.to_owned();
    for (name, value) in [
        ("{channel_id}", READ_CHANNEL),
        ("{message_id}", "3333333333"),
        ("{conversation_id}", "conversation-1"),
        ("{ticket}", ticket),
        ("{name}", "icon-192.png"),
    ] {
        let value = if broken == Some(name) {
            UNDECODABLE
        } else {
            value
        };
        filled = filled.replace(name, value);
    }
    filled
}

/// The well-formed request for `route`, and every malformed variant of it, from one caller.
fn requests(route: &Route, token: Option<&'static str>, ticket: &str) -> (Sent, Vec<Sent>) {
    let path = fill(route.path, ticket, None);
    let (good_query, bad_query) = match route.input {
        Input::Query(good, bad) => (good, Some(bad)),
        _ => ("", None),
    };
    let (content_type, good_body) = match route.input {
        Input::Json(body) | Input::OwnJson(body) | Input::Bytes(body, _) => {
            (Some("application/json"), body)
        }
        _ => (None, ""),
    };
    let well_formed = Sent {
        uri: format!("{path}{good_query}"),
        token,
        content_type,
        body: good_body.to_owned(),
        parsed: None,
    };
    let variant = |uri: String, content_type, body: &str, parsed| Sent {
        uri,
        token,
        content_type,
        body: body.to_owned(),
        parsed: Some(parsed),
    };
    let mut malformed = Vec::new();
    // Each path parameter, undecodable. A ticket is the credential itself, so an undecodable one
    // is simply an unknown ticket: it is only ever sent by a caller without one.
    for name in ["{channel_id}", "{message_id}", "{conversation_id}"] {
        if route.path.contains(name) {
            let broken = fill(route.path, ticket, Some(name));
            malformed.push(variant(
                format!("{broken}{good_query}"),
                content_type,
                good_body,
                StatusCode::BAD_REQUEST,
            ));
        }
    }
    if let Some(bad) = bad_query {
        malformed.push(variant(
            format!("{path}{bad}"),
            None,
            "",
            StatusCode::BAD_REQUEST,
        ));
    }
    match route.input {
        Input::Json(body) | Input::OwnJson(body) => {
            let wrong_shape = if matches!(route.input, Input::Json(_)) {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_REQUEST
            };
            malformed.push(variant(
                path.clone(),
                content_type,
                "not json",
                StatusCode::BAD_REQUEST,
            ));
            malformed.push(variant(path.clone(), content_type, "7", wrong_shape));
            malformed.push(variant(
                path.clone(),
                None,
                body,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ));
        }
        Input::Bytes(_, limit) => {
            malformed.push(variant(
                path.clone(),
                content_type,
                &"x".repeat(limit + 1),
                StatusCode::PAYLOAD_TOO_LARGE,
            ));
        }
        Input::Nothing | Input::Query(..) => {}
    }
    (well_formed, malformed)
}

/// The callers that may NOT use a route, and the status each must get.
fn refused_callers(auth: Auth) -> Vec<(Option<&'static str>, StatusCode)> {
    let unauthenticated = [
        (None, StatusCode::UNAUTHORIZED),
        (Some(WRONG_TOKEN), StatusCode::UNAUTHORIZED),
    ];
    match auth {
        Auth::Public => Vec::new(),
        Auth::Read => unauthenticated.to_vec(),
        Auth::Write => {
            let mut callers = unauthenticated.to_vec();
            callers.push((Some(READ_TOKEN), StatusCode::FORBIDDEN));
            callers
        }
        // The browser tokens are not ingest credentials.
        Auth::Ingest => {
            let mut callers = unauthenticated.to_vec();
            callers.push((Some(READ_TOKEN), StatusCode::UNAUTHORIZED));
            callers.push((Some(WRITE_TOKEN), StatusCode::UNAUTHORIZED));
            callers
        }
        // Any bearer token or none: only the ticket counts, and a guessed one is unknown.
        Auth::Ticket => vec![
            (None, StatusCode::NOT_FOUND),
            (Some(WRITE_TOKEN), StatusCode::NOT_FOUND),
        ],
    }
}

/// Every header credential that may use a route. A ticket route ignores the header entirely.
fn authorized_tokens(auth: Auth) -> Vec<Option<&'static str>> {
    match auth {
        Auth::Read => vec![Some(READ_TOKEN), Some(WRITE_TOKEN)],
        Auth::Write => vec![Some(WRITE_TOKEN)],
        Auth::Ingest => vec![Some(INGEST_TOKEN)],
        Auth::Ticket => vec![None, Some(WRITE_TOKEN)],
        Auth::Public => Vec::new(),
    }
}

#[tokio::test]
async fn a_caller_that_may_not_use_a_route_gets_the_same_refusal_whatever_it_sends() {
    let harness = harness();
    let guessed = "a".repeat(43);
    let mut checked = 0;
    // Every offending request, not just the first, so one run names every route that regressed.
    let mut offences = Vec::new();
    for route in inventory() {
        for (token, refused) in refused_callers(route.auth) {
            let (well_formed, mut malformed) = requests(&route, token, &guessed);
            if route.auth == Auth::Ticket {
                malformed.push(Sent {
                    uri: fill(route.path, UNDECODABLE, None),
                    ..well_formed.clone()
                });
            }
            let expected = answer(&harness, route.method, &well_formed).await;
            if expected.0 != refused {
                offences.push(format!(
                    "{} {} from {token:?}: expected {refused}, got {} {}",
                    route.method, well_formed.uri, expected.0, expected.1
                ));
            }
            for sent in malformed {
                let got = answer(&harness, route.method, &sent).await;
                if got != expected {
                    offences.push(format!(
                        "{} {} from {token:?} with {:?} {:?}: got {} {}, but a well-formed \
                         request got {} {}",
                        route.method,
                        sent.uri,
                        sent.content_type,
                        sent.body.chars().take(40).collect::<String>(),
                        got.0,
                        got.1,
                        expected.0,
                        expected.1
                    ));
                }
                checked += 1;
            }
        }
    }
    assert!(
        offences.is_empty(),
        "{} requests were not refused like a well-formed one:\n{}",
        offences.len(),
        offences.join("\n")
    );
    assert!(
        checked > 200,
        "only {checked} malformed requests were checked"
    );
}

#[tokio::test]
async fn an_authorized_caller_still_gets_the_parsers_answer() {
    // The other half: the credential check did not swallow malformed requests. A caller who MAY
    // use the route is told exactly what is wrong with its request, with the same status as before
    // the check moved — 400, 413, 415 or 422 — and never a credential refusal.
    let harness = harness();
    let ticket = harness.tickets.mint(Prepared {
        channel: READ_CHANNEL.to_owned(),
        message: "3333333333".to_owned(),
        said: "hello".to_owned(),
        speed: None,
        observation: "observation-1".to_owned(),
        preparation_ms: 0,
    });
    for route in inventory() {
        for token in authorized_tokens(route.auth) {
            let (_, malformed) = requests(&route, token, &ticket);
            for sent in malformed {
                let (status, body) = answer(&harness, route.method, &sent).await;
                assert_eq!(
                    Some(status),
                    sent.parsed,
                    "{} {} from {token:?} with {:?} {:?}: {body}",
                    route.method,
                    sent.uri,
                    sent.content_type,
                    sent.body.chars().take(40).collect::<String>()
                );
            }
        }
    }
    // A media element may ask with HEAD, holding only the ticket: it gets what GET gets.
    let play = Sent {
        uri: format!("/api/v1/speech/{ticket}?selection=a"),
        token: None,
        content_type: None,
        body: String::new(),
        parsed: None,
    };
    let (get_status, _) = answer(&harness, "GET", &play).await;
    let (head_status, _) = answer(&harness, "HEAD", &play).await;
    assert_ne!(get_status, StatusCode::UNAUTHORIZED);
    assert_eq!(head_status, get_status, "HEAD with a live ticket");
}

#[tokio::test]
async fn every_path_answers_only_the_methods_in_the_inventory() {
    // The inventory is checked against the router's SOURCE above; this checks it against the
    // router's BEHAVIOUR, which no way of registering a route can hide from. Every method the
    // inventory does not list for a path must be refused by routing itself with 405 — asked with
    // the write token, which passes the door in front of routing, so routing is what answers.
    // (`HEAD` is left out because axum answers it wherever it answers `GET`, and `CONNECT` because
    // it needs an authority-form target rather than a path.)
    let harness = harness();
    let routes = inventory();
    let paths: BTreeSet<&str> = routes.iter().map(|route| route.path).collect();
    let mut offences = Vec::new();
    for path in paths {
        let uri = fill(path, "a", None);
        for method in ["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "TRACE"] {
            if routes
                .iter()
                .any(|route| route.path == path && route.method == method)
            {
                continue;
            }
            let sent = Sent {
                uri: uri.clone(),
                token: Some(WRITE_TOKEN),
                content_type: None,
                body: String::new(),
                parsed: None,
            };
            let (status, _) = answer(&harness, method, &sent).await;
            if status != StatusCode::METHOD_NOT_ALLOWED {
                offences.push(format!("{method} {uri}: {status}"));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "methods the router serves that the inventory does not list: {offences:?}"
    );
}

#[tokio::test]
async fn a_caller_without_a_token_cannot_map_the_api() {
    // Routing answers before any handler, so without the door in front of it an unknown path was
    // 404 and an unserved method 405 with an `allow` header, while a real route said 401: enough
    // to enumerate every route and method. Now all of them get the real route's 401, byte for byte,
    // and a caller WITH a token still gets routing's 404 and 405.
    let harness = harness();
    let get = |uri: &str, token| Sent {
        uri: uri.to_owned(),
        token,
        content_type: None,
        body: String::new(),
        parsed: None,
    };
    // The reference is a 401 a ROUTE produced, not the layer: the ingest route checks its own
    // token, and is the one route an anonymous caller reaches.
    let ingest = Sent {
        uri: "/api/v1/live/events".to_owned(),
        token: None,
        content_type: Some("application/json"),
        body: "{}".to_owned(),
        parsed: None,
    };
    let refusal = respond(&harness, "POST", &ingest).await;
    assert_eq!(refusal.0, StatusCode::UNAUTHORIZED, "{refusal:?}");
    let layer = respond(&harness, "GET", &get("/api/v1/channels", None)).await;
    assert_eq!(
        layer, refusal,
        "the layer's 401 differs from the route's own"
    );
    let mut offences = Vec::new();
    let routes = inventory();
    let paths: BTreeSet<&str> = routes
        .iter()
        .filter(|route| route.path.starts_with("/api/"))
        .map(|route| route.path)
        .collect();
    let mut probes = Vec::new();
    for path in paths {
        for method in ["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "TRACE"] {
            if !routes
                .iter()
                .any(|route| route.path == path && route.method == method)
            {
                probes.push((
                    method,
                    fill(path, "a", None),
                    StatusCode::METHOD_NOT_ALLOWED,
                ));
            }
        }
    }
    for uri in [
        "/api/",
        "/api/v1",
        "/api/v1/nope",
        "/api/v2/channels",
        "/api/v1/channels/1111111111/nope",
        "/api/v1/speech/a/nope",
        "/api/v1/live/events/nope",
        "/api/v1/speech/",
    ] {
        for method in ["GET", "POST"] {
            probes.push((method, uri.to_owned(), StatusCode::NOT_FOUND));
        }
    }
    for (method, uri, routed) in probes {
        for token in [None, Some(WRONG_TOKEN)] {
            let got = respond(&harness, method, &get(&uri, token)).await;
            if got != refusal {
                offences.push(format!("{method} {uri} from {token:?}: {got:?}"));
            }
        }
        let (status, body) = answer(&harness, method, &get(&uri, Some(READ_TOKEN))).await;
        if status != routed {
            offences.push(format!(
                "{method} {uri} with the read token: expected {routed}, got {status} {body}"
            ));
        }
    }
    assert!(
        offences.is_empty(),
        "{} probes answered differently:\n{}",
        offences.len(),
        offences.join("\n")
    );
}

/// The headers a request is answered with, without reading the body: a granted stream or audio read
/// would never end. `None` if the router has not answered within ten seconds.
async fn headers_of(
    harness: &Harness,
    method: &str,
    sent: &Sent,
) -> Option<(StatusCode, axum::http::HeaderMap)> {
    let mut builder = Request::builder().method(method).uri(&sent.uri);
    if let Some(token) = sent.token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(content_type) = sent.content_type {
        builder = builder.header("content-type", content_type);
    }
    let request = builder
        .body(Body::from(sent.body.clone()))
        .expect("request");
    let answered = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        harness.router.clone().oneshot(request),
    )
    .await
    .ok()?
    .expect("router responds");
    Some((answered.status(), answered.headers().clone()))
}

#[tokio::test]
async fn every_api_answer_is_no_store_whoever_asks_and_however() {
    // `#223 asset-caching` made the page's static files cacheable. Nothing under `/api/` may follow
    // them: its answers are channel text, transcripts and credentials belonging to one caller, and
    // an installed app must never keep them in a browser cache. So every route in the inventory
    // that is not the public web app is asked well and badly, by callers who may use it and
    // callers who may not, and every answer — 200, 4xx, 5xx — must say `no-store`.
    let harness = harness();
    let ticket = harness.tickets.mint(Prepared {
        channel: READ_CHANNEL.to_owned(),
        message: "3333333333".to_owned(),
        said: "hello".to_owned(),
        speed: None,
        observation: "observation-1".to_owned(),
        preparation_ms: 0,
    });
    let mut offences = Vec::new();
    let mut checked = 0;
    let mut granted = 0;
    for route in inventory() {
        if route.auth == Auth::Public {
            assert!(!route.path.starts_with("/api/"), "{} is public", route.path);
            continue;
        }
        let callers = authorized_tokens(route.auth).into_iter().chain(
            refused_callers(route.auth)
                .into_iter()
                .map(|(token, _)| token),
        );
        for token in callers {
            let (well_formed, malformed) = requests(&route, token, &ticket);
            for sent in std::iter::once(well_formed).chain(malformed) {
                let Some((status, headers)) = headers_of(&harness, route.method, &sent).await
                else {
                    offences.push(format!(
                        "{} {} from {token:?}: no answer",
                        route.method, sent.uri
                    ));
                    continue;
                };
                checked += 1;
                if status.is_success() {
                    granted += 1;
                }
                let cache_control: Vec<_> = headers
                    .get_all("cache-control")
                    .iter()
                    .map(|value| value.to_str().unwrap_or("<opaque>").to_owned())
                    .collect();
                if cache_control != ["no-store"] || headers.contains_key("etag") {
                    offences.push(format!(
                        "{} {} from {token:?}: {status} cache-control {cache_control:?}, etag {:?}",
                        route.method,
                        sent.uri,
                        headers.get("etag")
                    ));
                }
            }
        }
    }
    assert!(
        offences.is_empty(),
        "{} API answers a browser could keep:\n{}",
        offences.len(),
        offences.join("\n")
    );
    // Guards on the sweep itself: refusals alone would make this pass with every handler broken.
    assert!(checked > 300, "only {checked} answers were checked");
    assert!(granted > 20, "only {granted} answers were successes");
}
