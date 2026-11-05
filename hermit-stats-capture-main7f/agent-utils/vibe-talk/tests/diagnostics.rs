//! `GET /api/v1/diagnostics`, driven through the real router.
//!
//! Every test here is written against a fake that **can say no**, and no two of them are the same
//! no. That is the whole design of this suite: a diagnostics route whose failure modes were only
//! ever exercised in one shape would be a route that reports one cause confidently and five
//! causes wrongly, which is strictly worse than the "unavailable" it replaces.
//!
//! What is NOT established here: anything about live Discord or live ElevenLabs. The status codes
//! and the endpoint shapes come from the vendors' documentation, and the fakes were written from
//! the same reading — so the two agree with each other and not yet with the vendors.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;
use vibe_talk::chat::{ChatClient, ChatError, ChatIdentity};
use vibe_talk::elevenlabs::fake::{KNOWN_AGENT_ID, KNOWN_VOICE_ID, VALID_API_KEY};
use vibe_talk::http::router;
use vibe_talk::model::{ChannelId, Message, MessageId};
use vibe_talk::state::AppState;
use vibe_talk::store::disabled::DisabledStore;
use vibe_talk::store::fake::FakeStore;
use vibe_talk::store::StateStore as _;
use vibe_talk::testing::{READ_TOKEN, WRITE_TOKEN};

const PATH: &str = "/api/v1/diagnostics";
const INGEST_TOKEN: &str = "diagnostics-ingest-token-that-must-stay-secret";

/// Ask for the report, as a caller with the read token would.
async fn report(state: AppState) -> Value {
    let (status, body, _headers) = ask(state, Some(READ_TOKEN)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the report IS the answer; a failing check must not become an HTTP failure: {body}"
    );
    body
}

async fn ask(state: AppState, token: Option<&str>) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut builder = Request::builder().method("GET").uri(PATH);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = router(state)
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("router responds");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value, headers)
}

/// The one check with this id. Panics rather than returning `None`, because a check that is
/// missing entirely is a failure of the route and not of the assertion that follows.
fn check<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .expect("checks is an array")
        .iter()
        .find(|check| check["id"] == id)
        .unwrap_or_else(|| panic!("no check with id {id}: {report:#}"))
}

/// Every check with this id — there is one per configured channel.
fn checks<'a>(report: &'a Value, id: &str) -> Vec<&'a Value> {
    report["checks"]
        .as_array()
        .expect("checks is an array")
        .iter()
        .filter(|check| check["id"] == id)
        .collect()
}

fn text(check: &Value, field: &str) -> String {
    check[field].as_str().unwrap_or_default().to_owned()
}

/// The channel ids in [`toml_with`]. Deliberately NOT the two the shared test scaffolding
/// registers on its fake Discord, so a server built from this configuration is one whose bot was
/// never given the channels — the most common way this deployment is wrong.
const UNINVITED_READ_CHANNEL: &str = "5555555555";
const UNINVITED_WRITE_CHANNEL: &str = "6666666666";

/// A configuration with a distinctive secret in every slot, so a leak is unmistakable.
fn toml_with(elevenlabs: &str) -> String {
    format!(
        r#"
[server]
bind = "127.0.0.1:0"

[discord]
bot_token = "{READ_TOKEN}-DISCORD-BOT-SECRET"

[auth]
read_token = "{READ_TOKEN}"
write_token = "{WRITE_TOKEN}"

[ingest]
token = "{INGEST_TOKEN}"

[[channels]]
id = "{UNINVITED_READ_CHANNEL}"
label = "build noise"
writable = false

[[channels]]
id = "{UNINVITED_WRITE_CHANNEL}"
label = "lead team"
writable = true

{elevenlabs}
"#
    )
}

// ---------------------------------------------------------------------------------------------
// The route itself
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_correctly_wired_deployment_reports_every_check_green() {
    let (state, _discord) = vibe_talk::testing::state();
    let body = report(state).await;
    assert_eq!(body["ok"], true, "{body:#}");
    assert_eq!(body["failed"], 0, "{body:#}");
    // Token, two channels, read-aloud selection, key, agent, voice, summaries, storage.
    assert_eq!(body["checks"].as_array().expect("checks").len(), 9);
    for check in body["checks"].as_array().expect("checks") {
        assert_eq!(check["status"], "pass", "{check:#}");
        // A check that passed must say WHAT it established, or it cannot be told from one that
        // did not run. For a channel that evidence is which channel — the subject and the label
        // — because "readable" about an unnamed channel is the ambiguity this route removes; for
        // everything else it is a sentence naming the account, the voice, or the file.
        if check["id"] == "discord.channel" {
            assert!(check["subject"].is_string(), "{check:#}");
        } else {
            assert!(
                !text(check, "detail").is_empty(),
                "a check that passed and says nothing about what it found: {check:#}"
            );
        }
    }
}

#[tokio::test]
async fn the_report_takes_the_read_scope_and_refuses_a_stranger() {
    let (state, _discord) = vibe_talk::testing::state();
    let (status, body, _headers) = ask(state.clone(), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body:#}");

    let (status, body, _headers) = ask(state.clone(), Some("Bearer-nonsense")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body:#}");

    // The WRITE token is a superset and must also work: an operator holding one credential should
    // not have to go and find the other to ask what is wrong.
    let (status, _body, _headers) = ask(state, Some(WRITE_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_report_is_never_cached() {
    // It names accounts, agent ids and file paths, and it is a point-in-time answer. Neither a
    // proxy nor a browser may keep it.
    let (state, _discord) = vibe_talk::testing::state();
    let (_status, _body, headers) = ask(state, Some(READ_TOKEN)).await;
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
}

#[tokio::test]
async fn every_failing_check_says_what_to_do_about_it() {
    // The property the route exists for. A red row with no remedy is the "unavailable" this
    // replaces, so it is asserted over a deployment that is wrong in FIVE ways at once.
    let (state, discord, store) =
        vibe_talk::testing::state_with_store_from_toml(&toml_with("[elevenlabs]\napi_key = \"xi-a-key-this-account-does-not-have\"\nagent_id = \"agent_not_on_this_account\""));
    // The channels are never registered on this fake, so both are unreachable.
    let _ = &discord;
    store.fail_next("the disk is full");
    let body = report(state).await;
    assert_eq!(body["ok"], false, "{body:#}");
    let failures: Vec<&Value> = body["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .filter(|check| check["status"] == "fail")
        .collect();
    assert!(failures.len() >= 4, "{body:#}");
    for check in failures {
        let remedy = text(check, "remedy");
        assert!(
            remedy.len() > 20,
            "a failing check with no usable remedy: {check:#}"
        );
        assert!(
            !text(check, "summary").is_empty(),
            "a failing check with no summary: {check:#}"
        );
    }
}

#[tokio::test]
async fn no_credential_reaches_the_report_even_when_the_vendor_quotes_one_back() {
    // Not hypothetical: this repository's ElevenLabs stand-in deliberately echoes the key it was
    // given into its 401 body, exactly as a real API is free to, so this drives the redaction
    // rather than reasoning about it.
    let key = "xi-a-wrong-key-that-must-never-be-echoed";
    let (state, _discord, _elevenlabs) = vibe_talk::testing::state_from_toml(&toml_with(&format!(
        "[elevenlabs]\napi_key = \"{key}\"\nagent_id = \"{KNOWN_AGENT_ID}\""
    )));
    let body = report(state).await;
    let rendered = body.to_string();
    for secret in [
        key,
        &format!("{READ_TOKEN}-DISCORD-BOT-SECRET"),
        READ_TOKEN,
        WRITE_TOKEN,
        INGEST_TOKEN,
    ] {
        assert!(
            !rendered.contains(secret),
            "a credential reached the diagnostics report: {secret}\n{body:#}"
        );
    }
    // And the redaction did not simply eat the answer.
    assert_eq!(check(&body, "elevenlabs.api_key")["status"], "fail");
    assert!(
        text(check(&body, "elevenlabs.api_key"), "summary").contains("401"),
        "{body:#}"
    );
}

#[tokio::test]
async fn the_report_redacts_a_credential_the_vendor_error_types_never_saw() {
    // The test above passes even with the report's OWN redaction deleted, because the ElevenLabs
    // error types redact at construction — which is a good thing about them and a blind spot in
    // that test. This one goes through a path where nothing upstream redacts anything: a store
    // backend error is free text from a backend, and `StoreError::Backend` does not know what a
    // secret is. A connection string or a path with a token in it arrives here verbatim.
    //
    // Written after a mutation run showed the earlier test staying green with
    // `diagnostics::redacted` reduced to the identity function.
    let store = Arc::new(FakeStore::new());
    let bot_token = format!("{READ_TOKEN}-DISCORD-BOT-SECRET");
    store.fail_next(&format!(
        "could not open the database: authentication failed for tokens {bot_token} and \
         {INGEST_TOKEN}"
    ));
    let (state, _discord, _elevenlabs) =
        vibe_talk::testing::state_from_toml(&toml_with("[elevenlabs]"));
    let state = AppState { store, ..state };
    let body = report(state).await;
    let storage = check(&body, "storage");
    assert_eq!(storage["status"], "fail", "{storage:#}");
    assert!(
        !body.to_string().contains(&bot_token),
        "a bot token reached the report through a store backend error, which nothing upstream \
         of the report redacts: {body:#}"
    );
    assert!(
        !body.to_string().contains(INGEST_TOKEN),
        "an ingest token reached the report through a store backend error, which nothing upstream \
         of the report redacts: {body:#}"
    );
    // The rest of the backend's sentence must survive, or the operator has been handed a
    // redaction instead of a diagnosis.
    assert!(
        text(storage, "detail").contains("could not open the database"),
        "{storage:#}"
    );
    assert!(
        text(storage, "detail").contains("<redacted>"),
        "{storage:#}"
    );
}

// ---------------------------------------------------------------------------------------------
// Discord: the token, and the five channel causes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_revoked_bot_token_is_reported_against_the_token_and_not_against_the_channels() {
    // The failure this check was added for. Without a call that names no channel, a bad token
    // arrives as "your channels are unreachable" and sends an operator to re-copy snowflakes that
    // were right all along.
    let (state, discord) = vibe_talk::testing::state();
    discord.revoke_token();
    let body = report(state).await;
    let token = check(&body, "discord.token");
    assert_eq!(token["status"], "fail", "{body:#}");
    assert!(text(token, "summary").contains("401"), "{token:#}");
    assert!(
        text(token, "remedy").contains("Reset Token"),
        "the remedy must name the click that fixes it: {token:#}"
    );
}

#[tokio::test]
async fn a_channel_the_bot_cannot_see_names_the_snowflake_the_label_and_the_invite() {
    // `state_from_toml` builds its fake Discord from scratch and registers only the channels the
    // shipped test fixture names. These two ids are not among them, so this is exactly the shape
    // of a bot that was invited to a server and never given the channel.
    let (state, _discord, _elevenlabs) = vibe_talk::testing::state_from_toml(&toml_with(&format!(
        "[elevenlabs]\napi_key = \"{VALID_API_KEY}\"\nagent_id = \"{KNOWN_AGENT_ID}\""
    )));
    let body = report(state).await;
    let channels = checks(&body, "discord.channel");
    assert_eq!(channels.len(), 2, "{body:#}");
    for channel in channels {
        assert_eq!(channel["status"], "fail", "{channel:#}");
        assert!(
            channel["subject"].is_string(),
            "a per-channel check must name WHICH channel: {channel:#}"
        );
    }
    let first = check(&body, "discord.channel");
    assert_eq!(first["subject"], UNINVITED_READ_CHANNEL);
    assert!(
        text(first, "title").contains("build noise"),
        "the label the operator wrote must appear: {first:#}"
    );
    assert!(
        text(first, "remedy").contains("Copy Channel ID"),
        "{first:#}"
    );
}

#[tokio::test]
async fn a_channel_added_in_the_app_is_included_in_on_demand_diagnostics() {
    let (state, discord, store) = vibe_talk::testing::state_with_store();
    let added = ChannelId("7777777777777777777".to_owned());
    discord.seed(&added, "codex-eng", "readable after restart");
    store
        .add_channel(&added, "added team", false, None, 1)
        .await
        .expect("store added channel");
    assert_eq!(
        state
            .restore_added_channels()
            .await
            .expect("restore channels"),
        1
    );

    let body = report(state).await;
    let channels = checks(&body, "discord.channel");
    assert_eq!(
        channels.len(),
        3,
        "diagnostics omitted the channel added in the app: {body:#}"
    );
    let added_check = channels
        .into_iter()
        .find(|check| check["subject"] == added.as_str())
        .unwrap_or_else(|| panic!("diagnostics did not name the added channel: {body:#}"));
    assert_eq!(added_check["status"], "pass", "{added_check:#}");
    assert!(
        text(added_check, "title").contains("added team"),
        "diagnostics lost the added channel label: {added_check:#}"
    );
}

#[tokio::test]
async fn a_channel_whose_messages_come_back_blank_warns_about_the_message_content_intent() {
    // The one misconfiguration Discord reports as a SUCCESS, and therefore the one a report is
    // most valuable for. A warning, not a failure: a message can legitimately be attachment-only.
    #[derive(Debug)]
    struct BlankContent;
    #[async_trait::async_trait]
    impl ChatClient for BlankContent {
        fn provider_name(&self) -> &str {
            "Discord"
        }

        async fn identity(&self) -> Result<ChatIdentity, ChatError> {
            Ok(ChatIdentity {
                id: "3000000000000000009".to_owned(),
                username: "blank-content-bot".to_owned(),
            })
        }
        async fn fetch_page(
            &self,
            channel: &ChannelId,
            _limit: u16,
            _before: Option<&MessageId>,
            _after: Option<&MessageId>,
        ) -> Result<Vec<Message>, ChatError> {
            Ok(vec![Message {
                thread: None,
                id: MessageId("1".to_owned()),
                channel_id: channel.clone(),
                author: "somebody".to_owned(),
                author_id: vibe_talk::model::UserId("2000000000000000001".to_owned()),
                author_is_bot: true,
                timestamp: "2026-08-18T12:00:00+00:00".to_owned(),
                spoken_time: String::new(),
                reply_to: None,
                content: String::new(),
                spoken_content: String::new(),
                content_html: String::new(),
                noise: false,
                reactions: None,
                heading: None,
            }])
        }
        async fn post_message(
            &self,
            _channel: &ChannelId,
            _content: &str,
            _reply_to: Option<&MessageId>,
        ) -> Result<Message, ChatError> {
            panic!("a diagnostics run must never post");
        }
    }

    let (mut state, _discord) = vibe_talk::testing::state();
    state.replace_chat(Arc::new(BlankContent));
    let body = report(state).await;
    assert_eq!(body["warned"], 2, "{body:#}");
    assert_eq!(
        body["ok"], true,
        "a blank backlog is suspicious, not provably wrong: {body:#}"
    );
    let first = check(&body, "discord.channel");
    assert_eq!(first["status"], "warn");
    assert!(
        text(first, "summary").contains("EMPTY CONTENT"),
        "{first:#}"
    );
    assert!(
        text(first, "remedy").contains("MESSAGE CONTENT INTENT"),
        "{first:#}"
    );
}

#[tokio::test]
async fn a_vendor_that_never_answers_is_a_failed_check_and_not_a_hung_request() {
    // The bound is the point. A route that makes live vendor calls and has no deadline is a route
    // that hangs exactly when the vendor is the thing that is wrong.
    #[derive(Debug)]
    struct NeverAnswers;
    #[async_trait::async_trait]
    impl ChatClient for NeverAnswers {
        async fn identity(&self) -> Result<ChatIdentity, ChatError> {
            std::future::pending().await
        }
        async fn fetch_page(
            &self,
            _channel: &ChannelId,
            _limit: u16,
            _before: Option<&MessageId>,
            _after: Option<&MessageId>,
        ) -> Result<Vec<Message>, ChatError> {
            std::future::pending().await
        }
        async fn post_message(
            &self,
            _channel: &ChannelId,
            _content: &str,
            _reply_to: Option<&MessageId>,
        ) -> Result<Message, ChatError> {
            panic!("a diagnostics run must never post");
        }
    }

    // A paused clock, so the budget elapses instantly and the suite stays fast. Without it this
    // test would take three times `CHECK_BUDGET` in wall time or would not be written at all.
    tokio::time::pause();
    let (mut state, _discord) = vibe_talk::testing::state();
    state.replace_chat(Arc::new(NeverAnswers));
    let finished = tokio::time::timeout(Duration::from_secs(300), report(state))
        .await
        .expect("the route must answer even when the vendor does not");
    let token = check(&finished, "discord.token");
    assert_eq!(token["status"], "fail", "{finished:#}");
    assert!(text(token, "summary").contains("time budget"), "{token:#}");
    assert!(
        text(token, "remedy").contains("egress"),
        "a timeout must still say what to do: {token:#}"
    );
    for channel in checks(&finished, "discord.channel") {
        assert_eq!(channel["status"], "fail", "{channel:#}");
    }
}

// ---------------------------------------------------------------------------------------------
// ElevenLabs: the key, the agent, the voice
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_accepted_key_says_which_workspace_it_reached() {
    // "The key works" is not the whole answer. A key that works against the WRONG workspace is
    // the ElevenLabs failure that reads exactly like a typo'd agent id, so the workspace is
    // reported on the passing path, not only on the failing one.
    let (state, _discord) = vibe_talk::testing::state();
    let body = report(state).await;
    let key = check(&body, "elevenlabs.api_key");
    assert_eq!(key["status"], "pass", "{key:#}");
    assert!(
        text(key, "detail").contains(vibe_talk::elevenlabs::fake::FAKE_WORKSPACE),
        "{key:#}"
    );
}

#[tokio::test]
async fn an_unconfigured_elevenlabs_names_the_setting_and_calls_nothing() {
    // Kept apart from a refusal on purpose: "the vendor said no" and "you never gave us a key"
    // look the same in a list of red rows and are fixed completely differently.
    let (state, _discord, elevenlabs) =
        vibe_talk::testing::state_from_toml(&vibe_talk::testing::config_toml_without_elevenlabs());
    let body = report(state).await;
    for id in ["elevenlabs.api_key", "elevenlabs.agent", "elevenlabs.voice"] {
        let found = check(&body, id);
        assert_eq!(found["status"], "fail", "{found:#}");
    }
    let key = check(&body, "elevenlabs.api_key");
    assert_eq!(key["unconfigured"], true, "{key:#}");
    assert_eq!(key["detail"], "elevenlabs.api_key", "{key:#}");
    assert!(
        text(key, "remedy").contains("elevenlabs.api_key"),
        "the remedy must name the setting: {key:#}"
    );
    assert_eq!(
        elevenlabs.attempts(),
        0,
        "nothing should be requested when there is no key to request it with"
    );
}

#[tokio::test]
async fn an_agent_this_account_does_not_have_is_told_apart_from_a_bad_key() {
    // The two 4xx answers from one vendor that send an operator to two different settings.
    let (state, _discord, _elevenlabs) = vibe_talk::testing::state_from_toml(&toml_with(&format!(
        "[elevenlabs]\napi_key = \"{VALID_API_KEY}\"\nagent_id = \"agent_belonging_to_someone_else\""
    )));
    let body = report(state).await;
    assert_eq!(
        check(&body, "elevenlabs.api_key")["status"],
        "pass",
        "the KEY is fine here, and saying otherwise sends the operator to rotate it: {body:#}"
    );
    let agent = check(&body, "elevenlabs.agent");
    assert_eq!(agent["status"], "fail", "{agent:#}");
    assert!(text(agent, "summary").contains("404"), "{agent:#}");
    assert!(
        text(agent, "remedy").contains("elevenlabs.agent_id"),
        "{agent:#}"
    );
    assert_eq!(agent["subject"], "agent_belonging_to_someone_else");
}

#[tokio::test]
async fn an_agent_with_no_voice_of_its_own_fails_both_the_agent_and_the_voice_check() {
    // Two rows rather than one, because the fix is on the agent and the consequence is on the
    // voice, and a reader who only saw the consequence would go and set `voice_id` — which works,
    // and hides an agent that will never speak in a call.
    let (state, _discord, elevenlabs) = vibe_talk::testing::state_from_toml(&toml_with(&format!(
        "[elevenlabs]\napi_key = \"{VALID_API_KEY}\"\nagent_id = \"agent_with_no_voice\""
    )));
    elevenlabs.register_voiceless_agent("agent_with_no_voice");
    let body = report(state).await;
    for id in ["elevenlabs.agent", "elevenlabs.voice"] {
        let found = check(&body, id);
        assert_eq!(found["status"], "fail", "{found:#}");
        assert!(
            text(found, "remedy").contains("elevenlabs.voice_id"),
            "{found:#}"
        );
    }
}

#[tokio::test]
async fn an_unset_voice_id_is_reported_as_borrowed_rather_than_missing() {
    // `elevenlabs.voice_id` is OPTIONAL, and this is the check that makes the claim checkable
    // instead of a sentence in a README: unset must read as "borrowing the agent's voice", never
    // as a gap.
    let (state, _discord) = vibe_talk::testing::state();
    let body = report(state).await;
    let voice = check(&body, "elevenlabs.voice");
    assert_eq!(voice["status"], "pass", "{voice:#}");
    let detail = text(voice, "detail");
    assert!(detail.contains("borrows"), "{voice:#}");
    assert!(detail.contains(KNOWN_VOICE_ID), "{voice:#}");
}

#[tokio::test]
async fn an_agent_that_could_not_be_read_does_not_make_the_voice_row_blame_the_voice() {
    // A wrong instruction is worse than a vague one. With no `voice_id` set, the voice is
    // borrowed from the agent — so when the AGENT lookup fails for a reason of its own, this row
    // knows nothing about whether a voice would have resolved. Printing "give the agent a voice"
    // over a rejected API key sends the reader to the wrong dashboard page entirely.
    let (state, _discord, _elevenlabs) = vibe_talk::testing::state_from_toml(&toml_with(&format!(
        "[elevenlabs]\napi_key = \"xi-a-key-this-account-does-not-have\"\nagent_id = \"{KNOWN_AGENT_ID}\""
    )));
    let body = report(state).await;
    let agent = check(&body, "elevenlabs.agent");
    let voice = check(&body, "elevenlabs.voice");
    assert_eq!(agent["status"], "fail", "{agent:#}");
    assert_eq!(voice["status"], "fail", "{voice:#}");
    assert_eq!(
        voice["summary"], agent["summary"],
        "the voice row must carry the REAL cause down, not invent one: {body:#}"
    );
    assert!(
        text(voice, "remedy").contains("elevenlabs.api_key"),
        "the remedy must point at the rejected key, not at a missing voice: {voice:#}"
    );
    assert!(
        !text(voice, "remedy").contains("give the agent a voice"),
        "{voice:#}"
    );
}

#[tokio::test]
async fn an_explicitly_configured_voice_is_reported_as_the_one_that_wins() {
    let (state, _discord, _elevenlabs) = vibe_talk::testing::state_from_toml(&toml_with(&format!(
        "[elevenlabs]\napi_key = \"{VALID_API_KEY}\"\nagent_id = \"{KNOWN_AGENT_ID}\"\nvoice_id = \"voice_chosen_by_hand\""
    )));
    let body = report(state).await;
    let voice = check(&body, "elevenlabs.voice");
    assert_eq!(voice["status"], "pass", "{voice:#}");
    assert!(
        text(voice, "detail").contains("voice_chosen_by_hand"),
        "{voice:#}"
    );
}

// ---------------------------------------------------------------------------------------------
// Device speech is separate from ElevenLabs configuration.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn browser_read_aloud_is_identified_without_claiming_device_playback_was_verified() {
    let config = format!(
        "{}\n[read_aloud]\nbackend = 'browser'\n",
        vibe_talk::testing::config_toml_without_elevenlabs()
    );
    let (state, _chat, elevenlabs) = vibe_talk::testing::state_from_toml(&config);
    let body = report(state).await;
    let selected = check(&body, "read_aloud");
    assert_eq!(selected["title"], "Read-aloud backend selection");
    assert_eq!(selected["subject"], "browser");
    assert_eq!(selected["status"], "pass");
    assert_eq!(selected["unconfigured"], false);
    let detail = text(selected, "detail");
    assert!(detail.contains("Device voice selected"), "{selected:#}");
    assert!(
        detail.contains("no ElevenLabs credentials are needed"),
        "{selected:#}"
    );
    assert!(
        detail.contains("cannot verify browser voice availability or audible playback"),
        "{selected:#}"
    );
    assert!(
        detail.contains("tap a message on the device"),
        "{selected:#}"
    );

    let voice = check(&body, "elevenlabs.voice");
    assert_eq!(voice["title"], "ElevenLabs voice");
    assert_eq!(voice["unconfigured"], true);
    assert_ne!(voice["title"], selected["title"]);
    assert!(elevenlabs.requested().is_empty());
    assert!(elevenlabs.spoken().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_server_with_no_storage_configured_names_the_setting_to_add() {
    let (state, _discord) = vibe_talk::testing::state_with(Arc::new(DisabledStore));
    let body = report(state).await;
    let storage = check(&body, "storage");
    assert_eq!(storage["status"], "fail", "{storage:#}");
    assert_eq!(storage["unconfigured"], true, "{storage:#}");
    assert_eq!(storage["detail"], "storage.path", "{storage:#}");
    assert!(
        text(storage, "remedy").contains("storage.path"),
        "{storage:#}"
    );
}

#[tokio::test]
async fn a_store_that_cannot_be_written_to_reports_the_reason_it_gave() {
    let store = Arc::new(FakeStore::new());
    store.fail_next("attempt to write a readonly database");
    let (state, _discord) = vibe_talk::testing::state_with(store);
    let body = report(state).await;
    let storage = check(&body, "storage");
    assert_eq!(storage["status"], "fail", "{storage:#}");
    assert_eq!(
        storage["unconfigured"], false,
        "a store that IS configured and broken must not read as one that was never set up: \
         {storage:#}"
    );
    assert!(
        text(storage, "detail").contains("readonly database"),
        "the backend's own reason is the useful part: {storage:#}"
    );
    assert!(text(storage, "remedy").contains("ABSOLUTE"), "{storage:#}");
}

#[tokio::test]
async fn the_storage_check_writes_nothing_durable() {
    // It is reached from a READ-scope route, and this server's standing rule is that no read-scope
    // credential causes a durable write. A canary row would break that for the sake of a
    // diagnostic.
    let temporary = vibe_talk::testing::TempDir::new("diagnostics-writable");
    let path = temporary.path().join("vibe-talk.sqlite3");
    let store = Arc::new(
        vibe_talk::store::sqlite::SqliteStore::open(&path, vibe_talk::store::Retention::default())
            .expect("a fresh store opens"),
    );
    let (state, _discord) = vibe_talk::testing::state_with(store.clone());

    let before = std::fs::metadata(&path).expect("the file exists").len();
    let body = report(state).await;
    assert_eq!(check(&body, "storage")["status"], "pass", "{body:#}");
    assert!(
        text(check(&body, "storage"), "detail").contains("nothing was written"),
        "{body:#}"
    );

    assert!(
        store
            .conversations()
            .await
            .expect("the store still reads")
            .is_empty(),
        "the writability check left a row behind"
    );
    assert_eq!(
        std::fs::metadata(&path).expect("the file exists").len(),
        before,
        "the writability check changed the database file"
    );
}

#[tokio::test]
async fn the_report_carries_the_summary_routes_counters_as_numbers_only() {
    // `#208 persistent-cache`. The milestone's claims are about what summary requests did not cost,
    // and the counters are how a deployed host shows it without logging a key or a word.
    let (state, discord, _store, summarizer) =
        vibe_talk::testing::state_with_summarizer(&vibe_talk::testing::config_toml());
    let channel = vibe_talk::model::ChannelId(vibe_talk::testing::READ_CHANNEL.to_owned());
    let id = discord
        .seed(&channel, "someone", &"a long message ".repeat(40))
        .0;
    let body = report(state.clone()).await;
    assert_eq!(body["summaries"]["requests"], 0, "{body:#}");
    for _ in 0..2 {
        vibe_talk::ops::summarize_message(
            &state,
            vibe_talk::auth::Scope::Write,
            vibe_talk::testing::READ_CHANNEL,
            &id,
            &[],
            None,
        )
        .await
        .expect("summarises");
    }
    assert_eq!(summarizer.calls(), 1);
    let body = report(state).await;
    let summaries = body["summaries"].as_object().expect("a summaries section");
    for name in [
        "requests",
        "hits",
        "hits_without_upstream",
        "shared_fetch_joins",
        "upstream_window_fetches",
        "misses",
        "generated",
        "regenerated_below_cap",
        "served_below_cap_read_scope",
    ] {
        assert!(summaries[name].is_u64(), "{name} is not a count: {body:#}");
    }
    assert_eq!(summaries.len(), 9, "only counts: {body:#}");
    assert_eq!(summaries["requests"], 2);
    assert_eq!(summaries["generated"], 1);
    assert_eq!(summaries["hits"], 1);
    assert_eq!(summaries["hits_without_upstream"], 1);
}
