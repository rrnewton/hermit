//! End-to-end tests of `GET /api/v1/signed-url`, driven through the real router against an
//! in-memory ElevenLabs.
//!
//! The route mints a credential. That makes its failure modes more interesting than its success:
//! an unauthenticated caller must be refused *before* anything is minted, a missing key must be
//! named rather than papered over, a vendor rejection must be surfaced rather than swallowed, and
//! the account API key must not appear in anything that leaves this process.
//!
//! The fake these run against can fail — it holds one API key and one agent id and refuses
//! anything else, the way ElevenLabs does. See `src/elevenlabs/fake.rs`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;
use vibe_talk::elevenlabs::fake::{FakeElevenLabs, KNOWN_AGENT_ID, VALID_API_KEY};
use vibe_talk::http::router;
use vibe_talk::testing::{READ_TOKEN, WRITE_TOKEN};

const ROUTE: &str = "/api/v1/signed-url";

struct Harness {
    router: axum::Router,
    elevenlabs: Arc<FakeElevenLabs>,
}

fn harness() -> Harness {
    let (state, _discord, elevenlabs) = vibe_talk::testing::state_parts();
    Harness {
        router: router(state),
        elevenlabs,
    }
}

/// A server whose operator never set an ElevenLabs API key.
fn harness_without_a_key() -> Harness {
    let (state, _discord, elevenlabs) =
        vibe_talk::testing::state_from_toml(&vibe_talk::testing::config_toml_without_elevenlabs());
    Harness {
        router: router(state),
        elevenlabs,
    }
}

/// A server configured with a key that this fake account does not have.
fn harness_with_a_wrong_key() -> Harness {
    let toml = vibe_talk::testing::config_toml().replace(VALID_API_KEY, "xi-a-revoked-key-value");
    let (state, _discord, elevenlabs) = vibe_talk::testing::state_from_toml(&toml);
    Harness {
        router: router(state),
        elevenlabs,
    }
}

/// Returns the status, the raw body text, and the parsed body. The RAW text is what the leak
/// assertions look at: a key hiding in an unexpected field would still be in the bytes.
async fn call(harness: &Harness, token: Option<&str>) -> (StatusCode, String, Value) {
    let mut builder = Request::builder().method("GET").uri(ROUTE);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = harness
        .router
        .clone()
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("router responds");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let text = String::from_utf8(bytes.to_vec()).expect("utf-8");
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, text, value)
}

#[tokio::test]
async fn a_write_scoped_caller_gets_the_minted_url_unchanged() {
    let harness = harness();
    // A distinctive value, so "passed through" is asserted rather than coincidentally matched.
    harness
        .elevenlabs
        .mint("wss://api.elevenlabs.io/v1/convai/conversation?agent_id=x&token=THE-ONE-MINTED");
    let (status, _text, payload) = call(&harness, Some(WRITE_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["signed_url"],
        "wss://api.elevenlabs.io/v1/convai/conversation?agent_id=x&token=THE-ONE-MINTED",
        "the URL the browser connects to must be the one ElevenLabs minted, byte for byte"
    );
    assert_eq!(payload["agent_id"], KNOWN_AGENT_ID);
    assert_eq!(harness.elevenlabs.attempts(), 1);
}

#[tokio::test]
async fn an_unauthenticated_caller_is_refused_and_nothing_is_minted() {
    // The security-critical case. An open /signed-url would hand anyone on the internet a working
    // conversation with the owner's agent — strictly worse than the public link that turning on
    // agent authentication just closed.
    let harness = harness();
    let (status, text, payload) = call(&harness, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["error"], "unauthenticated");
    assert_eq!(
        harness.elevenlabs.attempts(),
        0,
        "an unauthenticated request must not even reach ElevenLabs"
    );
    assert!(
        !text.contains("wss://"),
        "a URL leaked to a stranger: {text}"
    );
    assert!(!text.contains(VALID_API_KEY), "leak: {text}");
}

#[tokio::test]
async fn a_bad_token_is_refused_and_nothing_is_minted() {
    let harness = harness();
    let (status, _text, payload) = call(&harness, Some("not-the-token-not-the-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["error"], "unauthenticated");
    assert_eq!(harness.elevenlabs.attempts(), 0);
}

#[tokio::test]
async fn the_read_token_cannot_mint_a_conversation() {
    // Minting is gated on the WRITE scope: the agent on the far end of the conversation holds a
    // credential of its own and can post in the owner's name, so this route has to be at least as
    // strong as the strongest thing the conversation can do.
    let harness = harness();
    let (status, _text, payload) = call(&harness, Some(READ_TOKEN)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(payload["error"], "forbidden");
    assert_eq!(harness.elevenlabs.attempts(), 0);
}

#[tokio::test]
async fn a_missing_api_key_is_named_not_papered_over() {
    let harness = harness_without_a_key();
    let (status, text, payload) = call(&harness, Some(WRITE_TOKEN)).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "an unconfigured server must not answer 200: {text}"
    );
    assert_eq!(payload["error"], "elevenlabs_not_configured");
    assert!(
        payload["detail"]
            .as_str()
            .expect("detail")
            .contains("elevenlabs.api_key"),
        "the operator must be told exactly which setting to set: {text}"
    );
    assert!(
        !text.contains("wss://"),
        "there must be no unsigned fallback URL: {text}"
    );
    assert_eq!(
        harness.elevenlabs.attempts(),
        0,
        "no call should be attempted without a key to attempt it with"
    );
}

#[tokio::test]
async fn an_elevenlabs_401_is_surfaced_honestly() {
    let harness = harness_with_a_wrong_key();
    let (status, text, payload) = call(&harness, Some(WRITE_TOKEN)).await;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "a vendor rejection is an upstream failure, not a success: {text}"
    );
    assert_eq!(payload["error"], "elevenlabs_error");
    let detail = payload["detail"].as_str().expect("detail");
    assert!(
        detail.contains("401"),
        "the vendor's status must survive to the operator: {detail}"
    );
    assert!(!text.contains("wss://"), "no fallback URL: {text}");
    assert_eq!(
        harness.elevenlabs.attempts(),
        1,
        "the call must actually have been made"
    );
}

#[tokio::test]
async fn an_unreachable_elevenlabs_is_a_bad_gateway_not_a_panic() {
    let harness = harness();
    harness.elevenlabs.fail_next("dns lookup failed");
    let (status, _text, payload) = call(&harness, Some(WRITE_TOKEN)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(payload["error"], "elevenlabs_error");
}

#[tokio::test]
async fn the_api_key_never_appears_in_any_answer() {
    // Every path this route can take, checked against the raw response bytes. The 401 case is the
    // dangerous one: the fake's rejection body QUOTES the key back, exactly as a real API is free
    // to do, so this is a live check of the redaction rather than a hypothetical one.
    let cases: Vec<(&str, Harness, Option<&str>)> = vec![
        ("success", harness(), Some(WRITE_TOKEN)),
        ("unauthenticated", harness(), None),
        ("read scope", harness(), Some(READ_TOKEN)),
        (
            "no key configured",
            harness_without_a_key(),
            Some(WRITE_TOKEN),
        ),
        ("vendor 401", harness_with_a_wrong_key(), Some(WRITE_TOKEN)),
    ];
    for (name, harness, token) in cases {
        let (_status, text, _payload) = call(&harness, token).await;
        assert!(
            !text.contains(VALID_API_KEY),
            "{name}: the ElevenLabs API key leaked into the response: {text}"
        );
        assert!(
            !text.contains("xi-a-revoked-key-value"),
            "{name}: the configured API key leaked into the response: {text}"
        );
        assert!(
            !text.contains(WRITE_TOKEN) && !text.contains("test-bot-token"),
            "{name}: another credential leaked into the response: {text}"
        );
    }
}

#[tokio::test]
async fn a_minted_url_is_never_cached() {
    // The minted URL is itself a bearer credential for the next fifteen minutes.
    let harness = harness();
    let response = harness
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(ROUTE)
                .header("authorization", format!("Bearer {WRITE_TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
}

#[tokio::test]
async fn the_provider_neutral_route_preserves_auth_and_describes_the_wire() {
    let harness = harness();
    let response = harness
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/voice-session")
                .header("authorization", format!("Bearer {WRITE_TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let payload: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(payload["protocol"], "elevenlabs");
    assert_eq!(payload["provider"], "ElevenLabs");
    assert_eq!(payload["input_sample_rate"], 16_000);
    assert_eq!(payload["output_sample_rate"], 16_000);
    assert!(payload["websocket_url"].as_str().is_some());

    let response = harness
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/client-config")
                .header("authorization", format!("Bearer {READ_TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("responds");
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let config: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(config["conversational_voice"]["name"], "ElevenLabs");
    assert_eq!(
        config["conversational_voice"]["instance_id"],
        KNOWN_AGENT_ID
    );
    assert_eq!(
        config["conversational_voice"]["settings_url"],
        format!("https://elevenlabs.io/app/agents/{KNOWN_AGENT_ID}")
    );
}

#[tokio::test]
async fn the_voice_page_is_public_code_and_carries_no_credential() {
    let harness = harness();
    for (path, needle) in [
        ("/voice", "<title>vibe-talk — voice</title>"),
        ("/voice.js", "/api/v1/voice-session"),
        // Loaded before voice.js, which decodes every server answer with it; without the route the
        // page would fail on its first `VibeTalkContract` reference.
        ("/contract.js", "globalThis.VibeTalkContract"),
        // The page LINKS this file. A missing route would not error anywhere — it would serve a
        // 404 into a <link> and render the app frame as a plain scrolling document, which is
        // exactly the regression the frame exists to remove, and it would look like a CSS bug.
        ("/voice.css", "100dvh"),
    ] {
        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("responds");
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let text = String::from_utf8(bytes.to_vec()).expect("utf-8");
        assert!(text.contains(needle), "{path} is not the page it should be");
        for secret in [VALID_API_KEY, WRITE_TOKEN, READ_TOKEN, "test-bot-token"] {
            assert!(!text.contains(secret), "{path} carries a credential");
        }
    }
}
