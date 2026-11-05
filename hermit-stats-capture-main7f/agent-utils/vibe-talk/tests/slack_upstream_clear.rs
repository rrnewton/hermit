//! `#258 slack-clear-unread`: clearing a Slack conversation's unread state, against a loopback fake
//! of the Slack Web API whose `conversations.mark` behaves as Slack's does, moving the cursor
//! wherever it is told, backwards too. Whatever keeps the cursor from going back is the client's.

mod slack_fake;

use std::sync::Arc;
use std::time::{Duration, Instant};

use slack_fake::{profiled, ts, Fake, FakeState, METHODS};
use vibe_talk::chat::{ChatClient, ChatError, UpstreamClearOutcome};
use vibe_talk::model::{ChannelId, MessageId};
use vibe_talk::slack::message_id_from_ts;
use vibe_talk::store::fake::FakeStore;
use vibe_talk::store::{StateStore as _, UpstreamClearMode, UpstreamClearResult};
use vibe_talk::upstream_clear::{classify, stored_floor, Candidates, Clearer, Timing};

const CH: &str = "C0000MAIN1";

fn channel() -> ChannelId {
    ChannelId(CH.to_owned())
}

fn mid(ts: &str) -> MessageId {
    message_id_from_ts(ts).expect("encodable ts")
}

/// RFC 3339 with microseconds, as the outcome writes a Slack `ts`.
fn instant(seconds: i64, micros: i64) -> String {
    let at = jiff::Timestamp::from_second(slack_fake::BASE_SECS + seconds).expect("instant");
    format!("{}.{micros:06}Z", at.strftime("%Y-%m-%dT%H:%M:%S"))
}

/// A conversation of ten messages, ten seconds apart, whose read cursor is at `last_read`.
async fn fake_with_cursor(last_read: Option<String>) -> Fake {
    let mut state = FakeState::default();
    let list = (0..10)
        .map(|i| profiled(&ts(i * 10, 0), "UADA00001", "Ada", &format!("message {i}")))
        .collect();
    state.history.insert(CH.to_owned(), list);
    if let Some(cursor) = last_read {
        state.last_read.insert(CH.to_owned(), cursor);
    }
    Fake::start(state, 20).await
}

fn cursor(fake: &Fake) -> Option<String> {
    fake.with(|state| state.last_read.get(CH).cloned())
}

fn marks(fake: &Fake) -> Vec<(String, String)> {
    fake.with(|state| state.marks.clone())
}

fn refusal(error: &ChatError) -> &str {
    match error.cause() {
        ChatError::Refused(reason) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

async fn clear(
    client: &impl ChatClient,
    through: &str,
    dry_run: bool,
) -> Result<UpstreamClearOutcome, ChatError> {
    client
        .clear_upstream_through(&channel(), &mid(through), dry_run)
        .await
}

#[tokio::test]
async fn it_is_off_unless_the_deployment_turns_it_on_and_then_asks_slack_nothing() {
    let fake = fake_with_cursor(Some(ts(0, 0))).await;
    let client = fake.client();
    assert!(!client.supports_upstream_clear());
    let error = clear(&client, &ts(50, 0), false)
        .await
        .expect_err("refused while off");
    assert!(refusal(&error).contains("upstream_clear = true"), "{error}");
    assert_eq!(
        classify(&Err(error)),
        UpstreamClearResult::Unavailable,
        "the loop stops rather than retrying"
    );
    assert!(
        fake.with(|state| state.requests.is_empty()),
        "Slack was asked"
    );
    assert!(fake.clearing_client().supports_upstream_clear());
}

#[tokio::test]
async fn a_newer_message_moves_the_cursor_and_reports_where_it_was() {
    let fake = fake_with_cursor(Some(ts(10, 0))).await;
    let outcome = clear(&fake.clearing_client(), &ts(40, 0), false)
        .await
        .expect("advanced");
    assert_eq!(
        outcome,
        UpstreamClearOutcome {
            scope: "conversation".to_owned(),
            target: instant(40, 0),
            previous: Some(instant(10, 0)),
            advanced: true,
            dry_run: false,
        }
    );
    assert_eq!(marks(&fake), vec![(CH.to_owned(), ts(40, 0))]);
    assert_eq!(cursor(&fake), Some(ts(40, 0)));
    let info = fake.requests("conversations.info");
    assert_eq!(info.len(), 1, "the cursor is read once, before the mark");
    assert_eq!(info[0].get("channel").map(String::as_str), Some(CH));
    for (method, _) in fake.with(|state| state.requests.clone()) {
        assert!(METHODS.contains(&method.as_str()), "{method}");
    }
}

#[tokio::test]
async fn a_microsecond_ts_is_marked_exactly() {
    let fake = fake_with_cursor(Some(ts(10, 0))).await;
    clear(&fake.clearing_client(), &ts(40, 800_199), false)
        .await
        .expect("advanced");
    assert_eq!(marks(&fake), vec![(CH.to_owned(), ts(40, 800_199))]);
}

#[tokio::test]
async fn a_message_at_or_before_the_cursor_is_already_read_and_nothing_is_written() {
    let fake = fake_with_cursor(Some(ts(50, 0))).await;
    let client = fake.clearing_client();
    for through in [ts(50, 0), ts(40, 0)] {
        let outcome = clear(&client, &through, false).await.expect("answered");
        assert!(!outcome.advanced, "{through}");
        assert!(!outcome.dry_run);
        assert_eq!(outcome.previous, Some(instant(50, 0)));
        assert_eq!(classify(&Ok(outcome)), UpstreamClearResult::AlreadyRead);
    }
    assert_eq!(fake.calls("conversations.mark"), 0);
    assert_eq!(cursor(&fake), Some(ts(50, 0)));
}

#[tokio::test]
async fn the_cursor_never_moves_backwards_after_the_owner_reads_further_in_slack() {
    let fake = fake_with_cursor(Some(ts(10, 0))).await;
    let client = fake.clearing_client();
    assert!(
        clear(&client, &ts(40, 0), false)
            .await
            .expect("first")
            .advanced
    );
    // The owner reads on in Slack's own client.
    fake.with(|state| {
        state.last_read.insert(CH.to_owned(), ts(90, 0));
    });
    let late = clear(&client, &ts(60, 0), false)
        .await
        .expect("a late push");
    assert!(!late.advanced);
    assert_eq!(late.previous, Some(instant(90, 0)));
    assert_eq!(
        cursor(&fake),
        Some(ts(90, 0)),
        "Slack would have moved it back; the client must not ask"
    );
    assert_eq!(marks(&fake).len(), 1);
}

#[tokio::test]
async fn a_dry_run_reads_the_cursor_and_writes_nothing() {
    let fake = fake_with_cursor(Some(ts(10, 0))).await;
    let client = fake.clearing_client();
    let would = clear(&client, &ts(40, 0), true).await.expect("dry run");
    assert_eq!(
        would,
        UpstreamClearOutcome {
            scope: "conversation".to_owned(),
            target: instant(40, 0),
            previous: Some(instant(10, 0)),
            advanced: true,
            dry_run: true,
        }
    );
    assert_eq!(classify(&Ok(would)), UpstreamClearResult::DryRun);
    let would_not = clear(&client, &ts(5, 0), true).await.expect("dry run");
    assert!(!would_not.advanced && would_not.dry_run);
    assert_eq!(fake.calls("conversations.mark"), 0);
    assert_eq!(fake.calls("conversations.info"), 2);
    assert_eq!(cursor(&fake), Some(ts(10, 0)));
}

#[tokio::test]
async fn a_conversation_never_read_has_no_previous_cursor() {
    let fake = fake_with_cursor(Some("0000000000.000000".to_owned())).await;
    let outcome = clear(&fake.clearing_client(), &ts(40, 0), false)
        .await
        .expect("advanced");
    assert!(outcome.advanced);
    assert_eq!(outcome.previous, None);
}

#[tokio::test]
async fn a_channel_narrowed_to_one_thread_is_refused_before_slack_is_asked() {
    let fake = fake_with_cursor(Some(ts(10, 0))).await;
    let thread = ChannelId(format!("{CH}~{}", ts(20, 0)));
    let error = fake
        .clearing_client()
        .clear_upstream_through(&thread, &mid(&ts(40, 0)), true)
        .await
        .expect_err("a thread cannot be cleared");
    let reason = refusal(&error);
    assert!(reason.contains("one thread"), "{reason}");
    assert!(
        reason.contains("no way to mark one thread read"),
        "{reason}"
    );
    assert_eq!(classify(&Err(error)), UpstreamClearResult::Unavailable);
    assert!(fake.with(|state| state.requests.is_empty()));
}

#[tokio::test]
async fn a_cursor_slack_did_not_report_is_refused_rather_than_marked_blind() {
    let fake = fake_with_cursor(None).await;
    let error = clear(&fake.clearing_client(), &ts(40, 0), false)
        .await
        .expect_err("no cursor");
    assert!(refusal(&error).contains("last_read"), "{error}");
    assert_eq!(fake.calls("conversations.mark"), 0);

    let fake = fake_with_cursor(Some(ts(10, 0))).await;
    fake.with(|state| state.not_member.push(CH.to_owned()));
    let error = clear(&fake.clearing_client(), &ts(40, 0), false)
        .await
        .expect_err("not a member");
    assert!(refusal(&error).contains("not a member"), "{error}");
    assert_eq!(fake.calls("conversations.mark"), 0);
}

#[tokio::test]
async fn refusals_no_retry_can_fix_stop_the_loop_with_a_reason() {
    for (method, code, words) in [
        ("conversations.mark", "unknown_method", "needs updating"),
        ("conversations.info", "unknown_method", "needs updating"),
        ("conversations.mark", "missing_scope", "channels:history"),
        ("conversations.mark", "not_in_channel", "not a member"),
        ("conversations.mark", "not_allowed_token_type", "user token"),
        ("conversations.info", "channel_not_found", "cannot find"),
        ("conversations.mark", "restricted_action", "turned off"),
        ("conversations.mark", "is_archived", "is_archived"),
    ] {
        let fake = fake_with_cursor(Some(ts(10, 0))).await;
        fake.with(|state| {
            state.fail.insert(method.to_owned(), code.to_owned());
        });
        let answer = clear(&fake.clearing_client(), &ts(40, 0), false).await;
        let error = answer.as_ref().expect_err("refused");
        assert!(
            refusal(error).contains(words),
            "{method} {code}: {}",
            refusal(error)
        );
        assert!(error.to_string().starts_with("Slack refused: "), "{error}");
        assert_eq!(
            classify(&answer),
            UpstreamClearResult::Unavailable,
            "{method} {code}"
        );
        assert_eq!(cursor(&fake), Some(ts(10, 0)));
    }
}

#[tokio::test]
async fn a_passing_failure_is_retried_by_the_loop_and_a_rate_limit_is_not_waited_out_here() {
    let fake = fake_with_cursor(Some(ts(10, 0))).await;
    fake.with(|state| {
        state.fail.insert(
            "conversations.mark".to_owned(),
            "outcome_unknown".to_owned(),
        );
    });
    let answer = clear(&fake.clearing_client(), &ts(40, 0), false).await;
    assert_eq!(classify(&answer), UpstreamClearResult::Failed, "{answer:?}");

    let fake = fake_with_cursor(Some(ts(10, 0))).await;
    fake.with(|state| state.rate_limit_once = Some(("conversations.mark".to_owned(), 7)));
    let started = Instant::now();
    let answer = clear(&fake.clearing_client(), &ts(40, 0), false).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a rate limit parked the serial push loop"
    );
    let error = answer.as_ref().expect_err("rate limited");
    assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));
    assert_eq!(classify(&answer), UpstreamClearResult::Failed);
    assert_eq!(fake.calls("conversations.mark"), 1);
}

/// The real loop, with no quiet period, over a client against the fake.
fn clearer(client: Arc<dyn ChatClient>, store: &Arc<FakeStore>) -> Clearer {
    Clearer {
        chat: client,
        store: store.clone(),
        candidates: Arc::new(Candidates::new()),
        timing: Timing {
            quiet: Duration::ZERO,
            max_wait: Duration::ZERO,
            retry_first: Duration::from_secs(60),
            retry_cap: Duration::from_secs(60),
        },
        label: Arc::new(|_| Some("agents".to_owned())),
        eligible: Arc::new(|_| true),
    }
}

async fn turn(clearer: &Clearer, channel: &ChannelId, mode: UpstreamClearMode) {
    let setting = clearer
        .store
        .set_upstream_clear_mode(channel, mode, 1)
        .await
        .expect("store");
    clearer
        .candidates
        .set_mode(channel, mode, stored_floor(&setting));
}

fn offer(clearer: &Clearer, channel: &ChannelId, seconds: i64) -> bool {
    clearer
        .candidates
        .offer_parts(channel, &mid(&ts(seconds, 0)), &instant(seconds, 0))
}

#[tokio::test]
async fn the_loop_clears_slack_through_the_newest_message_and_records_what_it_found() {
    let fake = fake_with_cursor(Some(ts(10, 0))).await;
    let store = Arc::new(FakeStore::new());
    let clearer = clearer(Arc::new(fake.clearing_client()), &store);
    turn(&clearer, &channel(), UpstreamClearMode::On).await;
    assert!(offer(&clearer, &channel(), 70));
    assert!(clearer.push_due().await);
    assert_eq!(marks(&fake), vec![(CH.to_owned(), ts(70, 0))]);
    let setting = &store.upstream_clear_settings().await.expect("read")[0];
    let pushed = setting.pushed.as_ref().expect("recorded");
    assert_eq!(pushed.result, UpstreamClearResult::Advanced);
    assert_eq!(pushed.target.as_deref(), Some(instant(70, 0).as_str()));
    assert_eq!(
        setting.previous_cursor.as_deref(),
        Some(instant(10, 0).as_str()),
        "Settings shows where Slack's marker was, for a manual undo"
    );
    assert!(
        !offer(&clearer, &channel(), 60),
        "an older message is never pushed"
    );
}

#[tokio::test]
async fn the_loop_stops_for_a_channel_slack_refuses_and_says_why() {
    let fake = fake_with_cursor(Some(ts(10, 0))).await;
    fake.with(|state| {
        state
            .fail
            .insert("conversations.mark".to_owned(), "unknown_method".to_owned());
    });
    let store = Arc::new(FakeStore::new());
    let clearer = clearer(Arc::new(fake.clearing_client()), &store);
    turn(&clearer, &channel(), UpstreamClearMode::On).await;
    assert!(offer(&clearer, &channel(), 70));
    assert!(clearer.push_due().await);
    let status = clearer.candidates.status(&channel());
    let reason = status.unavailable.expect("unavailable");
    assert!(reason.contains("conversations.mark"), "{reason}");
    assert!(!status.pending);
    assert!(
        !offer(&clearer, &channel(), 80),
        "nothing more is pushed until the setting is saved again"
    );
    assert!(!clearer.push_due().await);
    assert_eq!(fake.calls("conversations.mark"), 1, "retried a refusal");
    let setting = &store.upstream_clear_settings().await.expect("read")[0];
    let last = setting.last.as_ref().expect("recorded");
    assert_eq!(last.result, UpstreamClearResult::Unavailable);
    assert!(setting.pushed.is_none());
}

#[tokio::test]
async fn a_dry_run_through_the_loop_writes_nothing_at_slack() {
    let fake = fake_with_cursor(Some(ts(10, 0))).await;
    let store = Arc::new(FakeStore::new());
    let clearer = clearer(Arc::new(fake.clearing_client()), &store);
    turn(&clearer, &channel(), UpstreamClearMode::DryRun).await;
    assert!(offer(&clearer, &channel(), 70));
    assert!(clearer.push_due().await);
    assert_eq!(fake.calls("conversations.mark"), 0);
    assert_eq!(cursor(&fake), Some(ts(10, 0)));
    let setting = &store.upstream_clear_settings().await.expect("read")[0];
    let last = setting.last.as_ref().expect("recorded");
    assert_eq!(last.result, UpstreamClearResult::DryRun);
    assert_eq!(last.previous.as_deref(), Some(instant(10, 0).as_str()));
}
