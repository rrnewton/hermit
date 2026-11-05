//! `#49 cached-summaries`: the four questions a summary cache has to be able to answer.
//!
//! Every one of them is about how OFTEN the summariser is called, so they are driven against
//! `FakeSummarizer`, which counts. Every one of them has a control that makes the count move,
//! because "it was called zero times" is satisfied by a server that never summarises at all.

use std::sync::Arc;

use vibe_talk::auth::Scope;
use vibe_talk::discord::fake::FakeDiscord;
use vibe_talk::model::{ChannelId, MessageId};
use vibe_talk::ops::{self, SummaryOutcome};
use vibe_talk::state::AppState;
use vibe_talk::store::fake::FakeStore;
use vibe_talk::store::StateStore as _;
use vibe_talk::summarize::fake::FakeSummarizer;
use vibe_talk::testing::{self, READ_CHANNEL};

/// A configuration with the summary thresholds a test chose.
fn toml_with(threshold: usize, target: usize, context: usize) -> String {
    format!(
        "{}\n[summaries]\nthreshold_chars = {threshold}\ntarget_chars = {target}\n\
         context_messages = {context}\n",
        testing::config_toml()
    )
}

struct Server {
    state: AppState,
    discord: Arc<FakeDiscord>,
    store: Arc<FakeStore>,
    summarizer: Arc<FakeSummarizer>,
    channel: ChannelId,
}

impl Server {
    fn seed(&self, content: &str) {
        self.discord.seed(&self.channel, "codex-eng", content);
    }

    async fn newest_id(&self) -> String {
        newest_id(&self.state).await
    }
}

fn new_server(threshold: usize, target: usize, context: usize) -> Server {
    let (state, discord, store, summarizer) =
        testing::state_with_summarizer(&toml_with(threshold, target, context));
    let channel = ChannelId(READ_CHANNEL.to_owned());
    // Three short neighbours, so a context window has something to pick up.
    for n in 0..3 {
        discord.seed(&channel, "codex-eng", &format!("short note {n}"));
    }
    Server {
        state,
        discord,
        store,
        summarizer,
        channel,
    }
}

/// A message comfortably over any threshold used here.
fn long_text(tag: &str) -> String {
    format!("{tag} ").repeat(120)
}

async fn newest_id(state: &AppState) -> String {
    ops::messages(state, READ_CHANNEL, None)
        .await
        .expect("reads")
        .messages
        .last()
        .expect("a message")
        .id
        .0
        .clone()
}

#[tokio::test]
async fn a_short_message_costs_no_summariser_call_at_all() {
    // The threshold is checked BEFORE the cache and before the summariser. A page that asked
    // about every row would otherwise spend a model call on every one-line message in a channel.
    let server = new_server(400, 160, 3);
    server.seed("the runner stalled");
    let id = server.newest_id().await;

    let ops::Summarised { outcome, .. } =
        ops::summarize_message(&server.state, Scope::Write, READ_CHANNEL, &id, &[], None)
            .await
            .expect("summarises");
    assert_eq!(outcome, SummaryOutcome::BelowThreshold);
    assert_eq!(
        server.summarizer.calls(),
        0,
        "a message shorter than the threshold reached the summariser anyway"
    );

    // THE CONTROL. The same message, the same code path, a threshold it clears — and now it IS
    // summarised. Without this the assertion above is satisfied by a server that summarises
    // nothing.
    let control = new_server(5, 4, 3);
    control.seed("the runner stalled");
    let id = control.newest_id().await;
    let ops::Summarised { outcome, .. } =
        ops::summarize_message(&control.state, Scope::Write, READ_CHANNEL, &id, &[], None)
            .await
            .expect("summarises");
    assert!(
        matches!(outcome, SummaryOutcome::Generated(_)),
        "the control must actually summarise: {outcome:?}"
    );
    assert_eq!(control.summarizer.calls(), 1);
}

#[tokio::test]
async fn asking_twice_generates_once() {
    let server = new_server(400, 160, 3);
    server.seed(&long_text("deploy"));
    let id = server.newest_id().await;

    let ops::Summarised { outcome: first, .. } =
        ops::summarize_message(&server.state, Scope::Write, READ_CHANNEL, &id, &[], None)
            .await
            .expect("summarises");
    let ops::Summarised {
        outcome: second, ..
    } = ops::summarize_message(&server.state, Scope::Write, READ_CHANNEL, &id, &[], None)
        .await
        .expect("summarises");

    assert!(matches!(first, SummaryOutcome::Generated(_)), "{first:?}");
    assert!(
        matches!(second, SummaryOutcome::Cached(_)),
        "the second ask must be served from the store: {second:?}"
    );
    assert_eq!(first.text(), second.text(), "the cache changed the answer");
    assert_eq!(
        server.summarizer.calls(),
        1,
        "the second ask reached the summariser, so nothing is being cached"
    );
}

#[tokio::test]
async fn changing_a_summary_setting_makes_every_old_summary_unreachable() {
    // THE CONTROL for the test above, and the whole reason `policy_version` exists. The same
    // message, the same store, a different target width — and the cached entry must NOT be
    // served, because it was produced under an instruction that no longer applies.
    let first = new_server(400, 160, 3);
    first.seed(&long_text("deploy"));
    let id = first.newest_id().await;
    ops::summarize_message(&first.state, Scope::Write, READ_CHANNEL, &id, &[], None)
        .await
        .expect("summarises");
    assert_eq!(first.summarizer.calls(), 1);

    // A second server over THE SAME store, differing only in target_chars.
    let wider_server = new_server(400, 161, 3);
    let mut wider = wider_server.state.clone();
    wider.store = first.store.clone();
    wider_server.seed(&long_text("deploy"));
    let wider_summarizer = wider_server.summarizer.clone();
    let wider_id = newest_id(&wider).await;

    let ops::Summarised { outcome, .. } =
        ops::summarize_message(&wider, Scope::Write, READ_CHANNEL, &wider_id, &[], None)
            .await
            .expect("summarises");
    assert!(
        matches!(outcome, SummaryOutcome::Generated(_)),
        "a changed policy served a summary produced under the old one: {outcome:?}"
    );
    assert_eq!(wider_summarizer.calls(), 1);
}

#[tokio::test]
async fn editing_the_message_upstream_regenerates_only_that_summary() {
    let server = new_server(400, 160, 3);
    server.seed(&long_text("deploy"));
    let untouched = server.newest_id().await;
    ops::summarize_message(
        &server.state,
        Scope::Write,
        READ_CHANNEL,
        &untouched,
        &[],
        None,
    )
    .await
    .expect("summarises");
    assert_eq!(server.summarizer.calls(), 1);

    // A SECOND message, cached too, so "only that summary" has something to be about.
    server.seed(&long_text("rollback"));
    let edited = server.newest_id().await;
    ops::summarize_message(
        &server.state,
        Scope::Write,
        READ_CHANNEL,
        &edited,
        &[],
        None,
    )
    .await
    .expect("summarises");
    assert_eq!(server.summarizer.calls(), 2);

    // Now the upstream EDIT: the same message id, different text. A cache keyed on the id alone
    // would serve the old summary here forever, and nothing about the id would say it is wrong.
    server
        .discord
        .edit(&MessageId(edited.clone()), &long_text("rollback reverted"));
    // ...and the server learns of it, as an adapter's pushed update tells it (the ingest route
    // forgets the message). Until it learns, a summary asked for within thirty seconds of the
    // page's read is answered from the text the page was shown, the window speech preparation
    // accepts; see `ops::summarize_message`. `#208 persistent-cache`.
    server
        .state
        .read_ahead
        .forget(&server.channel, &MessageId(edited.clone()));
    let ops::Summarised { outcome, .. } = ops::summarize_message(
        &server.state,
        Scope::Write,
        READ_CHANNEL,
        &edited,
        &[],
        None,
    )
    .await
    .expect("summarises");
    assert!(
        matches!(outcome, SummaryOutcome::Generated(_)),
        "an edited message was served the summary of its old text: {outcome:?}"
    );
    assert_eq!(server.summarizer.calls(), 3);

    let ops::Summarised { outcome: again, .. } = ops::summarize_message(
        &server.state,
        Scope::Write,
        READ_CHANNEL,
        &untouched,
        &[],
        None,
    )
    .await
    .expect("summarises");
    assert!(
        matches!(again, SummaryOutcome::Cached(_)),
        "the untouched message's summary was invalidated too: {again:?}"
    );
    assert_eq!(server.summarizer.calls(), 3);
}

#[tokio::test]
async fn the_summariser_is_shown_fenced_text_and_the_context_it_was_configured_for() {
    // A summariser is a model being fed channel text. Being short is not an exemption from the
    // boundary the MCP path already enforces.
    //
    // EVERY assertion here is on what the summariser was HANDED — `RecordedRequest::prompt`,
    // captured inside `FakeSummarizer::summarize` — and none of it is on a prompt this test
    // built. That distinction is the whole test: the previous version constructed the prompt in
    // the test body and asserted on that, which held even while the shipped call site passed the
    // summariser raw content and the fencing helper had no caller at all.
    let server = new_server(400, 160, 2);
    let hostile = format!("{}{}", long_text("deploy"), vibe_talk::untrusted::FENCE);
    server.seed(&hostile);
    let id = server.newest_id().await;
    ops::summarize_message(&server.state, Scope::Write, READ_CHANNEL, &id, &[], None)
        .await
        .expect("summarises");

    let asked = server.summarizer.requests();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].context, 2,
        "the configured context window has to be the one that travels"
    );
    assert_eq!(asked[0].target_chars, 160);

    let prompt = &asked[0].prompt;
    assert!(
        prompt.starts_with(vibe_talk::summarize::PROMPT),
        "the instruction has to sit OUTSIDE the fence: {}",
        &prompt[..80.min(prompt.len())]
    );
    assert!(
        prompt.contains(vibe_talk::untrusted::NOTICE),
        "the summariser was handed channel text without the untrusted-content framing: {prompt}"
    );
    assert!(
        prompt.contains("[fence-marker-removed]"),
        "a forged fence in the message survived into the prompt intact: {prompt}"
    );
    assert!(
        !prompt.contains(&hostile),
        "the message reached the summariser with its forged fence untouched"
    );
    // The context messages travel inside the fence too, not merely counted: the two short notes
    // before the target are third-party text on exactly the same terms.
    assert!(
        prompt.contains("short note 1") && prompt.contains("short note 2"),
        "the context window was counted but not actually shown to the summariser: {prompt}"
    );
    assert_eq!(
        prompt.matches(vibe_talk::untrusted::FENCE).count(),
        4,
        "context and target are each fenced exactly once, and neither closes its own fence: \
         {prompt}"
    );
}

#[tokio::test]
async fn a_read_scope_caller_is_served_from_the_cache_and_never_writes_to_it() {
    // The read token is the one pasted into a hosted voice agent. It may SPEND the cache and it
    // may never FILL it: a durable write reachable from the least-trusted credential is what the
    // two-token split exists to prevent.
    let server = new_server(400, 160, 3);
    server.seed(&long_text("deploy"));
    let id = server.newest_id().await;

    for _ in 0..2 {
        let ops::Summarised { outcome, .. } =
            ops::summarize_message(&server.state, Scope::Read, READ_CHANNEL, &id, &[], None)
                .await
                .expect("a read token may ask");
        assert!(
            matches!(outcome, SummaryOutcome::Generated(_)),
            "a read-scope ask was served from a cache it must not have been able to fill: \
             {outcome:?}"
        );
    }
    assert_eq!(
        server.summarizer.calls(),
        2,
        "the second read-scope ask hit a cached entry, so the first one wrote to the store"
    );

    // THE CONTROL, twice over. The same message, the same store, a WRITE-scope caller: it fills
    // the cache...
    let ops::Summarised {
        outcome: filled, ..
    } = ops::summarize_message(&server.state, Scope::Write, READ_CHANNEL, &id, &[], None)
        .await
        .expect("summarises");
    assert!(matches!(filled, SummaryOutcome::Generated(_)), "{filled:?}");
    assert_eq!(server.summarizer.calls(), 3);

    // ...and the read token is then served FROM it, which is the half of the rule that makes the
    // assertions above about the WRITE rather than about a store that never works.
    let ops::Summarised { outcome: spent, .. } =
        ops::summarize_message(&server.state, Scope::Read, READ_CHANNEL, &id, &[], None)
            .await
            .expect("summarises");
    assert!(
        matches!(spent, SummaryOutcome::Cached(_)),
        "a read token must still be served from an entry someone else filed: {spent:?}"
    );
    assert_eq!(
        server.summarizer.calls(),
        3,
        "a cache hit still reached the summariser"
    );
}

#[tokio::test]
async fn a_store_that_cannot_cache_degrades_to_generating_rather_than_refusing() {
    // The cache is an optimisation. Making a READ fail because the OPTIONAL durable store is not
    // configured would turn storage into a hard dependency of summarising, which it is not.
    let server = new_server(400, 160, 3);
    let mut state = server.state.clone();
    state.store = Arc::new(vibe_talk::store::disabled::DisabledStore);
    server.seed(&long_text("deploy"));
    let id = newest_id(&state).await;

    for _ in 0..2 {
        let ops::Summarised { outcome, .. } =
            ops::summarize_message(&state, Scope::Write, READ_CHANNEL, &id, &[], None)
                .await
                .expect("an unconfigured store must not make a read fail");
        assert!(
            matches!(outcome, SummaryOutcome::Generated(_)),
            "{outcome:?}"
        );
    }
    assert_eq!(
        server.summarizer.calls(),
        2,
        "with no cache every ask must really generate; a 1 here would mean it cached somewhere"
    );
}

#[tokio::test]
async fn a_summariser_that_refuses_is_reported_rather_than_papered_over() {
    let server = new_server(400, 160, 3);
    server.seed(&long_text("deploy"));
    let id = server.newest_id().await;
    server.summarizer.fail_next("the model host is down");

    let error = ops::summarize_message(&server.state, Scope::Write, READ_CHANNEL, &id, &[], None)
        .await
        .expect_err("must not invent a summary");
    assert_eq!(error.code(), "summarizer_error", "{error}");

    // One-shot, so the recovery is asserted too: a failure must not disable summarising forever.
    let ops::Summarised { outcome, .. } =
        ops::summarize_message(&server.state, Scope::Write, READ_CHANNEL, &id, &[], None)
            .await
            .expect("summarises");
    assert!(
        matches!(outcome, SummaryOutcome::Generated(_)),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_message_outside_the_window_or_the_allowlist_is_refused_before_anything_is_summarised() {
    let server = new_server(400, 160, 3);
    assert_eq!(
        ops::summarize_message(
            &server.state,
            Scope::Write,
            "999999",
            "1000000000000000001",
            &[],
            None
        )
        .await
        .expect_err("must refuse")
        .code(),
        "unknown_channel"
    );
    assert_eq!(
        ops::summarize_message(
            &server.state,
            Scope::Write,
            READ_CHANNEL,
            "1000000000000009999",
            &[],
            None
        )
        .await
        .expect_err("must refuse")
        .code(),
        "unknown_message"
    );
    assert_eq!(server.summarizer.calls(), 0);
}

/// A row the page drew as ONE message is summarised as one piece of writing.
///
/// A post over Discord's length limit arrives as a message plus a short remainder, and the page
/// combines the two into a single row. Summarising only the first describes the half that stops
/// mid-sentence, and the remainder on its own is the back half of a sentence whose front half is
/// somewhere else — the least summarisable text in the channel.
///
/// Asserted on WHAT THE SUMMARISER WAS HANDED rather than on what it answered, because the fake
/// deliberately does not echo its input: a test comparing answers would pass for a server that
/// parsed the other ids and then dropped them.
#[tokio::test]
async fn a_row_that_combines_two_messages_is_summarised_from_the_whole_of_it() {
    let server = new_server(400, 160, 3);
    server.seed(&long_text("head"));
    let head = server.newest_id().await;
    server.seed("TAILMARKER and that is the end of it.");
    let tail = server.newest_id().await;

    let outcome = ops::summarize_message(
        &server.state,
        Scope::Write,
        READ_CHANNEL,
        &head,
        std::slice::from_ref(&tail),
        None,
    )
    .await
    .expect("summarises");
    assert!(
        matches!(outcome.outcome, SummaryOutcome::Generated(_)),
        "the combined row was not summarised at all"
    );

    let asked = server.summarizer.requests();
    assert_eq!(
        asked.len(),
        1,
        "one row must cost one call, not one per message"
    );
    assert!(
        asked[0].target.contains("head"),
        "the head of the row never reached the summariser: {:?}",
        asked[0].target
    );
    assert!(
        asked[0].target.contains("TAILMARKER"),
        "the trailing overflow was dropped, so half the row was summarised: {:?}",
        asked[0].target
    );
    // Joined with a newline, and in reading order. A tail concatenated onto the head with nothing
    // between them would run two sentences together and change what the text says.
    assert!(
        asked[0].target.find("head").unwrap() < asked[0].target.find("TAILMARKER").unwrap(),
        "the row was assembled out of order: {:?}",
        asked[0].target
    );
    assert!(
        asked[0].target.contains("\nTAILMARKER"),
        "no separator: {:?}",
        asked[0].target
    );
}

/// The tail is not a row of its own, so it is never summarised on its own account.
#[tokio::test]
async fn the_combined_row_is_cached_under_the_text_it_actually_summarised() {
    let server = new_server(400, 160, 3);
    server.seed(&long_text("head"));
    let head = server.newest_id().await;
    server.seed("TAILMARKER and that is the end of it.");
    let tail = server.newest_id().await;
    let with_tail = std::slice::from_ref(&tail);

    let one = |also: &'static [String]| {
        ops::summarize_message(&server.state, Scope::Write, READ_CHANNEL, &head, also, None)
    };
    let _ = one(&[]).await.expect("summarises");
    let first_calls = server.summarizer.requests().len();

    // The SAME primary message, now drawn as a bigger row. That is different text, so it must not
    // be served the answer cached for the smaller one — the cache key hashes the combined content
    // precisely so that a regrouping cannot return a stale summary of half the row.
    let regrouped = ops::summarize_message(
        &server.state,
        Scope::Write,
        READ_CHANNEL,
        &head,
        with_tail,
        None,
    )
    .await
    .expect("summarises");
    assert!(
        matches!(regrouped.outcome, SummaryOutcome::Generated(_)),
        "the regrouped row was served the summary of its first half"
    );
    assert_eq!(
        server.summarizer.requests().len(),
        first_calls + 1,
        "the combined row did not reach the summariser"
    );

    // ...and asking for the same combined row again IS a cache hit, so the key is stable.
    let again = ops::summarize_message(
        &server.state,
        Scope::Write,
        READ_CHANNEL,
        &head,
        with_tail,
        None,
    )
    .await
    .expect("summarises");
    assert!(
        matches!(again.outcome, SummaryOutcome::Cached(_)),
        "the same combined row was summarised twice"
    );
}

#[tokio::test]
async fn the_cache_tests_exercise_the_key_a_real_deployment_writes() {
    // Every assertion in this file runs through `FakeSummarizer`, which deliberately borrows the
    // SHIPPED backend slug rather than inventing one. Point it anywhere else and all of them stay
    // green while exercising a cache key no deployment ever writes — which is verbatim the
    // silent-stale-summary failure the version string exists to prevent, and nothing else in this
    // suite would notice. This is the one that does.
    let server = new_server(400, 160, 3);
    let prefix = format!("v2-{}-", vibe_talk::summarize::agent::BACKEND);
    assert!(
        server.state.summary_version.starts_with(&prefix),
        "these tests file summaries under {:?}, which does not start with the prefix a real \
         deployment writes ({prefix})",
        server.state.summary_version
    );
}

// --- #208 persistent-cache, milestone summary-keep -------------------------------------------
//
// What a summary request does NOT cost: a chat call for a row the page has just read, a second
// window fetch for rows asked about together, a model call after a restart or a change of a
// setting that changes no word of a summary. Each asserted against `FakeDiscord::fetch_count` and
// the summariser's own count, with the counters diagnostics reports checked beside them.

/// The key a summary of `id`, whose text is `content`, is filed under on `server`.
fn key_of(server: &Server, id: &str, content: &str) -> vibe_talk::store::SummaryKey {
    vibe_talk::store::SummaryKey {
        channel: server.channel.clone(),
        message: MessageId(id.to_owned()),
        content_hash: vibe_talk::summarize::content_hash(content),
        version: server.state.summary_version.to_string(),
    }
}

async fn summarise(server: &Server, scope: Scope, id: &str) -> ops::Summarised {
    ops::summarize_message(&server.state, scope, READ_CHANNEL, id, &[], None)
        .await
        .expect("summarises")
}

#[tokio::test]
async fn a_summary_asked_right_after_the_page_read_costs_no_fetch() {
    let server = new_server(400, 160, 3);
    server.seed(&long_text("deploy"));
    // The page reads the channel: one fetch, and the rows are now in the read-ahead memory.
    let id = server.newest_id().await;
    let after_page = server.discord.fetch_count();

    // The first summary generates. The target is recalled; the store misses; the context needs the
    // window in order, so one fetch is made (the page's own read did not go through the turn).
    let first = summarise(&server, Scope::Write, &id).await;
    assert!(
        matches!(first.outcome, SummaryOutcome::Generated(_)),
        "{first:?}"
    );
    assert_eq!(server.summarizer.calls(), 1);
    let after_first = server.discord.fetch_count();
    assert!(
        after_first <= after_page + 1,
        "{after_page} then {after_first}"
    );

    // Asked again within thirty seconds — a page reload in summary mode — it is a hit with no
    // chat call at all, for either scope.
    for scope in [Scope::Write, Scope::Read] {
        let again = summarise(&server, scope, &id).await;
        assert!(
            matches!(again.outcome, SummaryOutcome::Cached(_)),
            "{again:?}"
        );
        assert_eq!(again.outcome.text(), first.outcome.text());
    }
    assert_eq!(
        server.discord.fetch_count(),
        after_first,
        "a hit on a row the page had just read went back to the chat provider"
    );
    assert_eq!(server.summarizer.calls(), 1);
    let counts = server.state.summary_counters.snapshot();
    assert_eq!(counts.requests, 3);
    assert_eq!(counts.hits, 2);
    assert_eq!(counts.hits_without_upstream, 2);
    assert_eq!(counts.misses, 1);
    assert_eq!(counts.generated, 1);
    assert_eq!(counts.upstream_window_fetches, 1);

    // THE CONTROL: with the memory gone (the purge clears it), the same hit reads the window.
    server.state.read_ahead.clear();
    let cold = summarise(&server, Scope::Write, &id).await;
    assert!(
        matches!(cold.outcome, SummaryOutcome::Cached(_)),
        "{cold:?}"
    );
    assert_eq!(server.discord.fetch_count(), after_first + 1);
    assert_eq!(
        server
            .state
            .summary_counters
            .snapshot()
            .hits_without_upstream,
        2
    );
}

#[tokio::test(start_paused = true)]
async fn summaries_that_miss_together_share_one_window_fetch() {
    // Ten long rows on screen, none of them in memory, none of them summarised: the page asks for
    // all ten at once. One window read answers every one of them.
    let server = new_server(400, 160, 3);
    let ids: Vec<String> = (0..10)
        .map(|n| {
            server
                .discord
                .seed(&server.channel, "codex-eng", &long_text(&format!("row{n}")))
                .0
        })
        .collect();
    let slow = Arc::new(testing::SlowChat::new(
        server.discord.clone(),
        std::time::Duration::from_millis(1_000),
    ));
    let mut state = server.state.clone();
    state.replace_chat(slow.clone());
    let answers = futures_util::future::join_all(
        ids.iter()
            .map(|id| ops::summarize_message(&state, Scope::Write, READ_CHANNEL, id, &[], None)),
    )
    .await;
    for answer in answers {
        let answer = answer.expect("summarises");
        assert!(
            matches!(answer.outcome, SummaryOutcome::Generated(_)),
            "{answer:?}"
        );
    }
    assert_eq!(
        slow.reads(),
        1,
        "ten summaries that missed together read the window apart"
    );
    assert_eq!(server.summarizer.calls(), 10);
    // Each was summarised with its own context, taken from the one window.
    assert!(server
        .summarizer
        .requests()
        .iter()
        .all(|asked| asked.context == 3));
    let counts = state.summary_counters.snapshot();
    assert_eq!(counts.upstream_window_fetches, 1);
    assert_eq!(counts.shared_fetch_joins, 9);
}

#[tokio::test]
async fn a_pushed_delete_is_not_summarised_from_memory() {
    const INGEST_TOKEN: &str = "summaries-ingest-token-000000000";
    let (state, discord, _store, summarizer) = testing::state_with_summarizer(&format!(
        "{}\n[ingest]\ntoken = \"{INGEST_TOKEN}\"\n",
        toml_with(400, 160, 3)
    ));
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let deleted = discord.seed(&channel, "codex-eng", &long_text("deleted")).0;
    let unpushed = discord
        .seed(&channel, "codex-eng", &long_text("unpushed"))
        .0;
    // The page reads the channel, and both rows are summarised and filed.
    newest_id(&state).await;
    for id in [&deleted, &unpushed] {
        ops::summarize_message(&state, Scope::Write, READ_CHANNEL, id, &[], None)
            .await
            .expect("summarises");
    }
    assert_eq!(summarizer.calls(), 2);

    // Both are deleted upstream; an adapter pushes the deletion of one.
    discord.delete(&MessageId(deleted.clone()));
    discord.delete(&MessageId(unpushed.clone()));
    let push = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/live/events")
        .header("authorization", format!("Bearer {INGEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::json!({
                "event_id": "delete-one",
                "historical": false,
                "kind": "delete",
                "channel_id": READ_CHANNEL,
                "message_id": deleted,
            })
            .to_string(),
        ))
        .expect("request");
    let pushed = tower::ServiceExt::oneshot(vibe_talk::http::router(state.clone()), push)
        .await
        .expect("router");
    assert_eq!(pushed.status(), axum::http::StatusCode::ACCEPTED);

    // The pushed one goes upstream and is not there.
    let error = ops::summarize_message(&state, Scope::Write, READ_CHANNEL, &deleted, &[], None)
        .await
        .expect_err("a pushed delete was summarised from memory");
    assert_eq!(error.code(), "unknown_message", "{error}");
    // THE CONTROL: the one nobody pushed is still answered from the page's thirty-second memory,
    // which is the window speech preparation accepts. The push is what made the difference.
    let unpushed_answer =
        ops::summarize_message(&state, Scope::Write, READ_CHANNEL, &unpushed, &[], None)
            .await
            .expect("served from memory");
    assert!(
        matches!(unpushed_answer.outcome, SummaryOutcome::Cached(_)),
        "{unpushed_answer:?}"
    );
    assert_eq!(summarizer.calls(), 2);
}

#[tokio::test]
async fn a_thread_summary_recalls_only_from_that_thread() {
    let server = new_server(400, 160, 3);
    let content = long_text("threaded");
    let id = server
        .discord
        .seed(&server.channel, "codex-eng", &content)
        .0;
    // What a thread view read in thread T, and a summary of it filed.
    let window = ops::messages(&server.state, READ_CHANNEL, None)
        .await
        .expect("reads");
    let message = window
        .messages
        .iter()
        .find(|m| m.id.0 == id)
        .expect("seeded")
        .clone();
    server.state.read_ahead.clear();
    server.state.read_ahead.remember(
        &server.channel,
        Some("opaque/thread-T"),
        std::slice::from_ref(&message),
        std::time::Instant::now(),
    );
    server
        .store
        .cache_summary(&key_of(&server, &id, &content), "filed in thread T", None)
        .await
        .expect("file");
    let fetched = server.discord.fetch_count();

    let in_t = ops::summarize_message_scoped(
        &server.state,
        Scope::Write,
        READ_CHANNEL,
        &id,
        &[],
        None,
        Some("opaque/thread-T"),
    )
    .await
    .expect("answered from memory");
    assert_eq!(
        in_t.outcome,
        SummaryOutcome::Cached("filed in thread T".to_owned())
    );
    assert_eq!(
        server.discord.fetch_count(),
        fetched,
        "a thread hit made a chat call"
    );

    // Another thread does not see what was read in T: it goes upstream, to a backend with no
    // thread lookups, and is refused there rather than answered from T's memory.
    let elsewhere = ops::summarize_message_scoped(
        &server.state,
        Scope::Write,
        READ_CHANNEL,
        &id,
        &[],
        None,
        Some("opaque/thread-U"),
    )
    .await
    .expect_err("thread U was answered from what thread T read");
    assert_eq!(elsewhere.code(), "refused", "{elsewhere}");
    assert_eq!(server.summarizer.calls(), 0);
}

#[tokio::test]
async fn a_miss_that_needs_no_context_is_generated_from_memory_without_a_fetch() {
    // `#208 persistent-cache`, milestone `summary-keep`. Configured for no context, or for a thread,
    // the text the page just read is everything the summariser would be shown, so a store miss is
    // generated from it with no read of the window. THE CONTROL: configured for context, the same
    // miss reads the window once, for the neighbours.
    for (context, fetches) in [(0, 0), (3, 1)] {
        let server = new_server(400, 160, context);
        server.seed(&long_text("deploy"));
        let id = server.newest_id().await;
        let fetched = server.discord.fetch_count();
        let answer = summarise(&server, Scope::Write, &id).await;
        assert!(
            matches!(answer.outcome, SummaryOutcome::Generated(_)),
            "{answer:?}"
        );
        assert_eq!(
            server.discord.fetch_count(),
            fetched + fetches,
            "context_messages = {context}"
        );
        let asked = server.summarizer.requests();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].context, context);
    }

    // A thread: what the thread view read is recalled, and the miss is generated and filed with no
    // lookup. This backend has no thread lookups, so one would have been refused.
    let server = new_server(400, 160, 3);
    let content = long_text("threaded");
    let id = server
        .discord
        .seed(&server.channel, "codex-eng", &content)
        .0;
    let message = ops::messages(&server.state, READ_CHANNEL, None)
        .await
        .expect("reads")
        .messages
        .into_iter()
        .find(|m| m.id.0 == id)
        .expect("seeded");
    server.state.read_ahead.clear();
    server.state.read_ahead.remember(
        &server.channel,
        Some("opaque/thread-T"),
        std::slice::from_ref(&message),
        std::time::Instant::now(),
    );
    let fetched = server.discord.fetch_count();
    let answer = ops::summarize_message_scoped(
        &server.state,
        Scope::Write,
        READ_CHANNEL,
        &id,
        &[],
        None,
        Some("opaque/thread-T"),
    )
    .await
    .expect("generated from memory");
    assert!(
        matches!(answer.outcome, SummaryOutcome::Generated(_)),
        "{answer:?}"
    );
    assert_eq!(
        server.discord.fetch_count(),
        fetched,
        "a thread miss made a chat call"
    );
    assert_eq!(server.summarizer.requests()[0].context, 0);
    assert!(server
        .store
        .cached_summary(&key_of(&server, &id, &content))
        .await
        .expect("read")
        .is_some());
}

/// A real SQLite store for `server`, so a row can be aged by hand and a restart reopens the file.
fn sqlite_store(
    dir: &testing::TempDir,
) -> (
    Arc<vibe_talk::store::sqlite::SqliteStore>,
    std::path::PathBuf,
) {
    let file = dir.path().join("summaries.sqlite3");
    let store = vibe_talk::store::sqlite::SqliteStore::open(&file, Default::default())
        .expect("open the store");
    (Arc::new(store), file)
}

fn summary_rows(file: &std::path::Path) -> i64 {
    rusqlite::Connection::open(file)
        .expect("open")
        .query_row("SELECT COUNT(*) FROM summaries", [], |row| row.get(0))
        .expect("count")
}

fn age_every_summary(file: &std::path::Path, made_at_ms: i64) {
    rusqlite::Connection::open(file)
        .expect("open")
        .execute("UPDATE summaries SET made_at_ms = ?1", [made_at_ms])
        .expect("age");
}

#[tokio::test]
async fn a_read_scope_hit_from_memory_writes_nothing() {
    let dir = testing::TempDir::new("summaries-read-scope-touch");
    let (store, file) = sqlite_store(&dir);
    let mut server = new_server(400, 160, 3);
    server.state.store = store.clone();
    let content = long_text("deploy");
    server.seed(&content);
    let id = server.newest_id().await;
    summarise(&server, Scope::Write, &id).await;
    let key = key_of(&server, &id, &content);
    let two_days_ago = vibe_talk::store::now_ms() - 2 * 86_400_000;
    age_every_summary(&file, two_days_ago);

    let rows = summary_rows(&file);
    let fetched = server.discord.fetch_count();
    let hit = summarise(&server, Scope::Read, &id).await;
    assert!(matches!(hit.outcome, SummaryOutcome::Cached(_)), "{hit:?}");
    assert_eq!(
        server.discord.fetch_count(),
        fetched,
        "the hit was not from memory"
    );
    let held = store
        .cached_summary(&key)
        .await
        .expect("read")
        .expect("there");
    assert_eq!(
        held.made_at_ms, two_days_ago,
        "a read-scope hit kept an entry alive"
    );
    assert_eq!(summary_rows(&file), rows, "a read-scope hit wrote a row");

    // THE CONTROL: a write-scope hit on the same old row keeps it alive, once.
    let hit = summarise(&server, Scope::Write, &id).await;
    assert!(matches!(hit.outcome, SummaryOutcome::Cached(_)), "{hit:?}");
    let held = store
        .cached_summary(&key)
        .await
        .expect("read")
        .expect("there");
    assert!(
        held.made_at_ms > vibe_talk::store::now_ms() - 60_000,
        "{held:?}"
    );
    assert_eq!(server.summarizer.calls(), 1);
}

#[tokio::test]
async fn a_restart_or_a_change_of_a_setting_that_changes_no_word_asks_for_nothing_again() {
    // `#257 summary-truncation`'s deploy swept every summary on its host, because the version
    // hashed a setting that changes no word of an answer. Now the threshold and the deadline are
    // out of the version, so a server started afresh with either changed — over the same file —
    // finds every summary where the last one left it, and asks the model for none of them.
    let dir = testing::TempDir::new("summaries-restart");
    let (store, file) = sqlite_store(&dir);
    let mut before = new_server(400, 160, 3);
    before.state.store = store;
    let content = long_text("deploy");
    before.seed(&content);
    let id = before.newest_id().await;
    summarise(&before, Scope::Write, &id).await;
    assert_eq!(before.summarizer.calls(), 1);

    let changed = format!(
        "{}\n[summaries]\nthreshold_chars = 300\ntarget_chars = 160\ncontext_messages = 3\n\
         reply_timeout_seconds = 45\n",
        testing::config_toml()
    );
    let (mut after, _discord, _store, summarizer) = testing::state_with_summarizer(&changed);
    assert_eq!(
        after.summary_version, before.state.summary_version,
        "the threshold or the deadline moved the cache key"
    );
    after.replace_chat(before.discord.clone());
    after.store = Arc::new(
        vibe_talk::store::sqlite::SqliteStore::open(&file, Default::default()).expect("reopen"),
    );
    let answer = ops::summarize_message(&after, Scope::Write, READ_CHANNEL, &id, &[], None)
        .await
        .expect("summarises");
    assert!(
        matches!(answer.outcome, SummaryOutcome::Cached(_)),
        "{answer:?}"
    );
    assert_eq!(summarizer.calls(), 0, "the restart asked the model again");
}

#[tokio::test]
async fn a_stored_answer_cut_below_a_raised_cap_is_asked_for_again_only_by_a_writer() {
    // A row filed cut at 100 characters — under a cap smaller than today's, as the rows the first
    // start re-keys can be. A read token is served the shorter text it has, with no model call; a
    // write token asks again and files the whole answer.
    let server = new_server(400, 160, 3);
    let content = long_text("deploy");
    server.seed(&content);
    let id = server.newest_id().await;
    let key = key_of(&server, &id, &content);
    server
        .store
        .cache_summary(&key, "An answer the old cap cut short…", Some(100))
        .await
        .expect("file");

    let read = summarise(&server, Scope::Read, &id).await;
    assert_eq!(
        read.outcome,
        SummaryOutcome::Cached("An answer the old cap cut short…".to_owned())
    );
    assert_eq!(
        server.summarizer.calls(),
        0,
        "a read token spent the model on a stored answer"
    );
    assert_eq!(
        server
            .state
            .summary_counters
            .snapshot()
            .served_below_cap_read_scope,
        1
    );

    let write = summarise(&server, Scope::Write, &id).await;
    assert!(
        matches!(write.outcome, SummaryOutcome::Generated(_)),
        "{write:?}"
    );
    assert_eq!(server.summarizer.calls(), 1);
    let refiled = server
        .store
        .cached_summary(&key)
        .await
        .expect("read")
        .expect("filed");
    assert_eq!(
        refiled.cut_at, None,
        "the whole answer replaced the cut one"
    );
    assert_eq!(
        server
            .state
            .summary_counters
            .snapshot()
            .regenerated_below_cap,
        1
    );

    let again = summarise(&server, Scope::Write, &id).await;
    assert!(
        matches!(again.outcome, SummaryOutcome::Cached(_)),
        "{again:?}"
    );
    assert_eq!(server.summarizer.calls(), 1);
}

#[tokio::test]
async fn a_lower_cap_cuts_what_it_serves_and_asks_for_nothing() {
    // An answer filed whole at well past today's cap (a cap that has since come down): served cut
    // at the cap in force, at a word boundary, from the store, with no model call.
    let server = new_server(400, 160, 3);
    let content = long_text("deploy");
    server.seed(&content);
    let id = server.newest_id().await;
    let long_answer = "The runner stalled overnight and nobody noticed. ".repeat(30);
    server
        .store
        .cache_summary(
            &key_of(&server, &id, &content),
            long_answer.trim_end(),
            None,
        )
        .await
        .expect("file");
    let served = summarise(&server, Scope::Write, &id).await;
    let text = served.outcome.text().expect("a summary").to_owned();
    let cap = vibe_talk::summarize::hard_cap_chars(160);
    assert!(text.chars().count() <= cap, "{}", text.chars().count());
    assert!(text.ends_with('…'), "{text:?}");
    assert!(
        long_answer.starts_with(text.trim_end_matches('…')),
        "{text:?}"
    );
    assert_eq!(server.summarizer.calls(), 0);
}

#[tokio::test]
async fn a_generated_answer_is_filed_whole_and_handed_out_cut() {
    let server = new_server(400, 160, 3);
    let answer = "The team found the storage leak in the build cache. ".repeat(20);
    server.summarizer.answer_with(&answer);
    let content = long_text("deploy");
    server.seed(&content);
    let id = server.newest_id().await;
    let generated = summarise(&server, Scope::Write, &id).await;
    let shown = generated.outcome.text().expect("a summary").to_owned();
    assert!(shown.chars().count() <= vibe_talk::summarize::hard_cap_chars(160));
    assert!(shown.ends_with('…'), "{shown:?}");
    let filed = server
        .store
        .cached_summary(&key_of(&server, &id, &content))
        .await
        .expect("read")
        .expect("filed");
    assert_eq!(
        filed.text,
        answer.trim_end(),
        "what is filed is the whole answer"
    );
    assert_eq!(filed.cut_at, None);
    // ...and serving it from the store shows exactly what generating it showed.
    let cached = summarise(&server, Scope::Write, &id).await;
    assert_eq!(cached.outcome, SummaryOutcome::Cached(shown));
}
