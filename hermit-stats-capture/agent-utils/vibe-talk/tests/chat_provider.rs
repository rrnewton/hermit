//! Provider names must survive the real HTTP client and both API error paths.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;
use vibe_talk::chat::{ChatClient, ChatError, RateLimitExhausted};
use vibe_talk::discord::http::HttpDiscordClient;
use vibe_talk::http::router;
use vibe_talk::model::{ChannelId, MessageId};
use vibe_talk::testing::{self, WRITE_CHANNEL, WRITE_TOKEN};

async fn upstream(status: StatusCode, body: String) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let app = axum::Router::new().fallback(move || {
        let body = body.clone();
        async move { (status, body) }
    });
    let task = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (base, task)
}

#[tokio::test]
async fn google_chat_metadata_and_failed_reads_and_sends_use_the_backend_name() {
    // A multibyte response also exercises truncation: slicing at byte 500 used to panic.
    let (base, task) = upstream(StatusCode::BAD_GATEWAY, "故障".repeat(300)).await;
    let mut config = testing::config();
    let discord = config.discord_mut().expect("discord provider");
    discord.api_base = base;
    discord.provider_name = "Google Chat".to_owned();
    let (mut state, _) = testing::state();
    state.replace_chat(Arc::new(HttpDiscordClient::new(discord).expect("client")));
    // Leave state.config at its default Discord value to prove that the trait supplies the name.
    let app = router(state);
    for (method, uri, body) in [
        ("GET", "/api/v1/client-config".to_owned(), None),
        (
            "GET",
            format!("/api/v1/channels/{WRITE_CHANNEL}/page"),
            None,
        ),
        (
            "POST",
            format!("/api/v1/channels/{WRITE_CHANNEL}/reply"),
            Some(r#"{"text":"hello"}"#),
        ),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(&uri)
            .header("authorization", format!("Bearer {WRITE_TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(body.unwrap_or_default()))
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value: Value = serde_json::from_slice(&bytes).expect("json");
        if uri == "/api/v1/client-config" {
            assert_eq!(status, StatusCode::OK);
            assert_eq!(value["chat_provider_name"], "Google Chat");
        } else {
            assert_eq!(
                status,
                if method == "POST" {
                    StatusCode::MULTI_STATUS
                } else {
                    StatusCode::BAD_GATEWAY
                },
                "{value}"
            );
            assert!(
                value["detail"]
                    .as_str()
                    .expect("detail")
                    .starts_with("Google Chat returned HTTP 502:"),
                "{value}"
            );
            assert!(
                !value.to_string().to_lowercase().contains("discord"),
                "{value}"
            );
            if method == "GET" {
                assert_eq!(value["error"], "chat_error");
            } else {
                assert_eq!(value["posted"], 0);
                assert_eq!(value["unsent"], "hello");
            }
        }
    }
    task.abort();
}

#[tokio::test]
async fn malformed_successes_and_refusals_retain_provider_and_classification() {
    let (base, task) = upstream(StatusCode::OK, "{}".to_owned()).await;
    let mut config = testing::config();
    let discord = config.discord_mut().expect("discord provider");
    discord.api_base = base;
    discord.provider_name = "Google Chat".to_owned();
    let client = HttpDiscordClient::new(discord).expect("client");
    let channel = ChannelId(WRITE_CHANNEL.to_owned());
    for error in [
        client.identity().await.expect_err("missing identity"),
        client
            .fetch_recent(&channel, 1)
            .await
            .expect_err("missing list"),
        client
            .post_message(&channel, "hello", None)
            .await
            .expect_err("missing message"),
    ] {
        assert!(matches!(error.cause(), ChatError::Shape(_)), "{error}");
        assert!(error
            .to_string()
            .starts_with("Google Chat response could not be understood:"));
    }
    let refused = client
        .mark_read_upstream(&channel, &MessageId("123".to_owned()))
        .await
        .expect_err("disabled");
    assert!(matches!(refused.cause(), ChatError::Refused(_)));
    assert!(refused.to_string().starts_with("Google Chat refused:"));
    task.abort();
}

#[test]
fn provider_context_preserves_rate_limit_backoff() {
    let wait = Duration::from_secs(45);
    let error = ChatError::RateLimited(RateLimitExhausted {
        provider: "discord",
        route: "GET /channels/123/messages".to_owned(),
        attempts: 1,
        waited: Duration::ZERO,
        budget: Duration::from_secs(30),
        retry_after: wait,
        global: false,
    })
    .with_provider("Google Chat");
    assert_eq!(error.retry_after(), Some(wait));
    assert!(error.to_string().starts_with("Google Chat RATE LIMIT"));
    assert!(!error.to_string().contains("discord"));
    assert!(ChatError::Transport("offline".to_owned())
        .with_provider("Google Chat")
        .to_string()
        .starts_with("Google Chat request failed:"));
}

#[test]
fn provider_configuration_defaults_and_validates_the_display_name() {
    let parse = |name: &str| {
        let toml = testing::config_toml()
            .replace("[discord]", &format!("[discord]\nprovider_name = {name}"));
        vibe_talk::config::Config::from_toml_and_env(&toml, &Default::default())
    };
    assert_eq!(
        testing::config()
            .discord()
            .expect("discord provider")
            .provider_name,
        "Discord"
    );
    assert_eq!(
        parse("\" Google Chat \"")
            .expect("valid")
            .discord()
            .expect("discord provider")
            .provider_name,
        "Google Chat"
    );
    assert!(parse("\" \"").is_err());
    assert!(parse("\"Google\\nChat\"").is_err());
}

/// `#195 send-resilience`. The page's idempotency key reaches a registration bridge as the post's
/// nonce — which the bridge contract makes its provider request id — and reaches it IDENTICALLY on
/// a second attempt at the same text, so a retry after a lost answer names the post the first
/// attempt may already have made. The 207 says whether the failure is worth retrying at all.
#[tokio::test]
async fn a_keyed_reply_reaches_a_bridge_under_the_same_nonce_on_every_attempt() {
    use std::sync::Mutex;

    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let seen = Arc::clone(&bodies);
    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let seen = Arc::clone(&seen);
        async move {
            let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let content = value["content"].as_str().unwrap_or_default().to_owned();
            let attempt = {
                let mut seen = seen.lock().expect("bodies");
                seen.push(value);
                seen.len()
            };
            if content == "refused words" {
                return (
                    StatusCode::BAD_REQUEST,
                    r#"{"message":"not allowed"}"#.to_owned(),
                );
            }
            if attempt == 1 {
                // The incident's shape: the upstream command timed out, and nobody knows whether
                // the message went out before it did.
                return (
                    StatusCode::BAD_GATEWAY,
                    r#"{"message":"upstream send timed out"}"#.to_owned(),
                );
            }
            let message = serde_json::json!({
                "id": format!("900{attempt}"), "channel_id": WRITE_CHANNEL, "content": content,
                "timestamp": "2026-10-04T10:20:00Z",
                "author": {"id": "7", "username": "bridge", "bot": true},
            });
            (StatusCode::OK, message.to_string())
        }
    });
    let task = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let mut config = testing::config();
    let discord = config.discord_mut().expect("discord provider");
    discord.api_base = base;
    discord.provider_name = "Google Chat".to_owned();
    discord.channel_registration = true;
    discord.thread_api = vibe_talk::threads::ThreadApi::Off;
    let (mut state, _) = testing::state();
    state.replace_chat(Arc::new(HttpDiscordClient::new(discord).expect("client")));
    let app = router(state);
    let call = |method: &'static str, uri: String, body: Option<Value>| {
        let app = app.clone();
        async move {
            let request = Request::builder()
                .method(method)
                .uri(&uri)
                .header("authorization", format!("Bearer {WRITE_TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
                .expect("request");
            let response = app.oneshot(request).await.expect("response");
            let status = response.status();
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("body")
                .to_bytes();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).expect("json"),
            )
        }
    };
    let reply = format!("/api/v1/channels/{WRITE_CHANNEL}/reply");
    let keyed = |text: &str| serde_json::json!({"text": text, "idempotency_key": "outgoing-key_1"});

    let (status, config_body) = call("GET", "/api/v1/client-config".to_owned(), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        config_body["idempotent_posts_supported"], true,
        "{config_body}"
    );
    // A bridge has a reply pointer of its own: its replies are not thread replies, and the flag
    // that says so is not sent. `#252 thread-button-state`.
    assert!(
        config_body.get("replies_in_thread").is_none(),
        "{config_body}"
    );

    let (status, first) = call("POST", reply.clone(), Some(keyed("the same words"))).await;
    assert_eq!(status, StatusCode::MULTI_STATUS, "{first}");
    assert_eq!(first["posted"], 0, "{first}");
    assert_eq!(
        first["retryable"], true,
        "a timed-out send was called final: {first}"
    );

    let (status, second) = call("POST", reply.clone(), Some(keyed("the same words"))).await;
    assert_eq!(status, StatusCode::OK, "{second}");

    let (status, _) = call("POST", reply.clone(), Some(keyed("other words"))).await;
    assert_eq!(status, StatusCode::OK);

    let (status, refused) = call("POST", reply.clone(), Some(keyed("refused words"))).await;
    assert_eq!(status, StatusCode::MULTI_STATUS, "{refused}");
    assert_eq!(
        refused["retryable"], false,
        "a refusal was offered for retry: {refused}"
    );

    let (status, bad) = call(
        "POST",
        reply.clone(),
        Some(serde_json::json!({"text": "x", "idempotency_key": "not a key!"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");

    let nonces: Vec<String> = bodies
        .lock()
        .expect("bodies")
        .iter()
        .map(|body| {
            body["nonce"]
                .as_str()
                .expect("every bridge post has a nonce")
                .to_owned()
        })
        .collect();
    assert_eq!(nonces.len(), 4, "{nonces:?}");
    assert_eq!(
        nonces[0], nonces[1],
        "a retry of the same text minted a different request id"
    );
    assert!(nonces[0].starts_with("outgoing-key_1."), "{nonces:?}");
    assert_ne!(
        nonces[0], nonces[2],
        "one key named two different texts as the same post"
    );
    task.abort();
}

#[test]
fn a_part_key_is_bound_to_its_text_and_stays_a_valid_bridge_nonce() {
    let key = "k".repeat(vibe_talk::ops::IDEMPOTENCY_KEY_MAX_CHARS);
    let first = vibe_talk::ops::part_key(&key, "a part", 0);
    assert_eq!(first, vibe_talk::ops::part_key(&key, "a part", 0));
    assert_ne!(first, vibe_talk::ops::part_key(&key, "another part", 0));
    assert_ne!(
        first,
        vibe_talk::ops::part_key(&key, "a part", 1),
        "a repeated part would be posted once instead of twice"
    );
    assert!(first.len() <= 128, "{first}");
    assert!(first
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')));
}

#[test]
fn only_a_bridge_whose_every_post_carries_the_nonce_promises_one_post_per_key() {
    use vibe_talk::threads::ThreadApi;

    let mut config = testing::config();
    let discord = config.discord_mut().expect("discord provider");
    discord.api_base = "http://127.0.0.1:9".to_owned();
    let client = |discord: &vibe_talk::config::DiscordConfig| {
        HttpDiscordClient::new(discord)
            .expect("client")
            .supports_idempotent_posts()
    };
    assert!(
        !client(discord),
        "a direct client has no request id to hold a key in"
    );
    discord.channel_registration = true;
    discord.thread_api = ThreadApi::Native;
    assert!(
        !client(discord),
        "native thread posts go to the provider itself, which ignores the nonce"
    );
    discord.thread_api = ThreadApi::Bridge;
    assert!(client(discord));
    discord.thread_api = ThreadApi::Off;
    assert!(client(discord));
}

/// Everything a recording bridge was asked to post: the path, and the JSON body.
type Posts = Arc<std::sync::Mutex<Vec<(String, Value)>>>;

/// vibe-talk in front of a registration bridge with the bridge thread API, the shape for which a
/// key is promised to post once. The bridge records every post and answers the ones numbered in
/// `fail` (from 1, across the whole test) with the incident's 502 — a send that timed out with
/// nobody knowing whether it went out first.
async fn recording_bridge(
    fail: &'static [usize],
) -> (axum::Router, Posts, tokio::task::JoinHandle<()>) {
    let posts: Posts = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let seen = Arc::clone(&posts);
    let bridge = axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
        let seen = Arc::clone(&seen);
        async move {
            let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let content = value["content"].as_str().unwrap_or_default().to_owned();
            let path = uri.path().to_owned();
            let count = {
                let mut seen = seen.lock().expect("posts");
                seen.push((path.clone(), value));
                seen.len()
            };
            if fail.contains(&count) {
                return (
                    StatusCode::BAD_GATEWAY,
                    r#"{"message":"upstream send timed out"}"#.to_owned(),
                );
            }
            let thread = path
                .split("/threads/")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
                .map(|id| {
                    serde_json::json!({
                        "id": id, "root_message_id": null, "is_root": false,
                        "reply_count": null, "reply_count_exact": false,
                    })
                });
            let message = serde_json::json!({
                "id": format!("9194{count:04}"), "channel_id": WRITE_CHANNEL, "content": content,
                "timestamp": "2026-10-04T10:20:00Z", "thread": thread,
                "author": {"id": "7", "username": "bridge", "bot": true},
            });
            (StatusCode::OK, message.to_string())
        }
    });
    let task = tokio::spawn(async move { axum::serve(listener, bridge).await.expect("serve") });
    let mut config = testing::config();
    let discord = config.discord_mut().expect("discord provider");
    discord.api_base = base;
    discord.provider_name = "Google Chat".to_owned();
    discord.channel_registration = true;
    discord.thread_api = vibe_talk::threads::ThreadApi::Bridge;
    let (mut state, _) = testing::state();
    state.replace_chat(Arc::new(HttpDiscordClient::new(discord).expect("client")));
    (router(state), posts, task)
}

/// One keyed reply into a thread, as the page sends it.
async fn keyed_thread_reply(app: &axum::Router, text: &str) -> (StatusCode, Value) {
    let body = serde_json::json!({
        "text": text, "thread_id": "t_194", "idempotency_key": "outgoing-key_3",
    });
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/channels/{WRITE_CHANNEL}/reply"))
        .header("authorization", format!("Bearer {WRITE_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, serde_json::from_slice(&bytes).expect("json"))
}

/// A 207 without its `unsent` text, which is pages long here, for an assertion to print.
fn brief(answer: &Value) -> Value {
    let mut answer = answer.clone();
    if let Some(fields) = answer.as_object_mut() {
        fields.remove("unsent");
    }
    answer
}

/// `#195 send-resilience`. The page sends a 207's `unsent` text again on its own where the 207
/// says `resumable`, and tells the reader that cannot post twice. Through the thread route the
/// incident's sends took, the post that failed has to go upstream again as the same words under
/// the same nonce — including for a long message whose fenced block sat in a part that DID post,
/// which once moved every later cut, and for a fenced message nothing of which posted.
#[tokio::test]
async fn a_resumable_remainder_goes_upstream_again_as_the_post_that_failed() {
    // Part 1 posts, part 2 fails, and the retry of the remainder succeeds.
    let (app, posts, task) = recording_bridge(&[2]).await;
    // No word longer than three letters, so a space falls inside any four characters: wherever
    // the four characters once reserved for a closing fence are, a cut is there to move.
    let prose: Vec<String> = (0..1500)
        .map(|i| format!("row {} is ok", i % 100))
        .collect();
    let text = format!("```sh\nls -l\n```\n\n{}", prose.join(" "));
    let (status, partial) = keyed_thread_reply(&app, &text).await;
    let shown = brief(&partial);
    assert_eq!(status, StatusCode::MULTI_STATUS, "{shown}");
    assert_eq!(partial["posted"], 1, "{shown}");
    assert_eq!(partial["retryable"], true, "{shown}");
    assert_eq!(
        partial["resumable"], true,
        "prose behind a posted fence was left to the reader: {shown}"
    );
    let unsent = partial["unsent"].as_str().expect("unsent").to_owned();
    let (status, done) = keyed_thread_reply(&app, &unsent).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    {
        let posts = posts.lock().expect("posts");
        assert!(posts.len() > 3, "the fixture did not split three ways");
        assert!(
            posts
                .iter()
                .all(|(path, _)| path.ends_with("/threads/t_194/messages")),
            "a post left the thread route"
        );
        let (failed, again) = (&posts[1].1, &posts[2].1);
        assert_eq!(
            again["content"], failed["content"],
            "the retry cut the failed part differently"
        );
        assert_eq!(
            again["nonce"], failed["nonce"],
            "the retry asked for the failed part under another request id"
        );
        assert!(failed["nonce"]
            .as_str()
            .is_some_and(|nonce| nonce.starts_with("outgoing-key_3.")));
    }
    task.abort();

    // A fenced message long enough to split, nothing of which posted: `unsent` is the text as
    // sent, so sending it again is the same request rather than the parts joined back up.
    let (app, posts, task) = recording_bridge(&[1]).await;
    let code = format!("```rust\n{}```", "let answer = 42;\n".repeat(300));
    let (status, partial) = keyed_thread_reply(&app, &code).await;
    let shown = brief(&partial);
    assert_eq!(status, StatusCode::MULTI_STATUS, "{shown}");
    assert_eq!(partial["posted"], 0, "{shown}");
    assert_eq!(partial["unsent"], code.as_str());
    assert_eq!(partial["resumable"], true, "{shown}");
    let (status, _) = keyed_thread_reply(&app, &code).await;
    assert_eq!(status, StatusCode::OK);
    {
        let posts = posts.lock().expect("posts");
        assert_eq!(posts[1].1["content"], posts[0].1["content"]);
        assert_eq!(posts[1].1["nonce"], posts[0].1["nonce"]);
    }
    task.abort();
}

/// `#195 send-resilience`. Where sending the remainder again would NOT repeat the failed post
/// exactly, the 207 says so, and the page leaves it to the reader: here the failed part opened
/// and closed inside a code block, so the fence markers the server added at its cuts come back
/// as ordinary text and the cuts move.
#[tokio::test]
async fn a_remainder_that_would_be_cut_differently_is_not_offered_as_resumable() {
    let (app, posts, task) = recording_bridge(&[2]).await;
    let code = format!("```rust\n{}```", "let answer = 42;\n".repeat(400));
    let (status, partial) = keyed_thread_reply(&app, &code).await;
    let shown = brief(&partial);
    assert_eq!(status, StatusCode::MULTI_STATUS, "{shown}");
    assert_eq!(partial["posted"], 1, "{shown}");
    assert_eq!(
        partial["retryable"], true,
        "the cause is still a moment: {shown}"
    );
    assert_eq!(
        partial["resumable"], false,
        "a remainder whose failed part would change was offered as safe: {shown}"
    );
    // What resending it WOULD have done, which is why it is not offered.
    let failed = posts.lock().expect("posts")[1].1.clone();
    let (status, _) = keyed_thread_reply(&app, partial["unsent"].as_str().expect("unsent")).await;
    assert_eq!(status, StatusCode::OK);
    let again = posts.lock().expect("posts")[2].1.clone();
    assert_ne!(
        again["nonce"], failed["nonce"],
        "the fixture no longer exercises a moved cut"
    );
    task.abort();
}
