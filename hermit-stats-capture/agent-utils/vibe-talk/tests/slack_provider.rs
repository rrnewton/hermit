//! `vibe_talk::slack::HttpSlackClient` against a loopback fake of the Slack Web API subset.
//!
//! The fake caps every page at a handful of entries, far below what the client asks for, because
//! a server is allowed to do that and the bridge this client is meant to work through does it.

mod slack_fake;

use std::time::{Duration, Instant};

use serde_json::json;
use slack_fake::{plain, profiled, system, thread, ts, Fake, FakeState, METHODS, SELF_USER, TOKEN};
use vibe_talk::chat::{ChatClient, ChatError};
use vibe_talk::directory::DirectoryRequest;
use vibe_talk::live::{poll_once, LiveHub};
use vibe_talk::model::{ChannelId, ChannelInfo, Message, MessageId};
use vibe_talk::probe::{classify, Diagnosis};
use vibe_talk::slack::{message_id_from_ts, ts_from_message_id, HttpSlackClient};
use vibe_talk::threads::{MessageThread, TimelineRequest, TimelineView};

const CH: &str = "C0000MAIN1";

fn channel() -> ChannelId {
    ChannelId(CH.to_owned())
}

fn ts_list(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| ts_from_message_id(&message.id).expect("a Slack-encoded id"))
        .collect()
}

fn mid(ts: &str) -> MessageId {
    message_id_from_ts(ts).expect("encodable ts")
}

/// `n` real messages ten seconds apart, with a join event after every seventh.
fn linear(n: usize) -> (FakeState, Vec<String>) {
    let mut state = FakeState::default();
    let mut list = Vec::new();
    let mut real = Vec::new();
    for i in 0..n {
        let index = i64::try_from(i).expect("small");
        let at = ts(index * 10, (index * 37) % 1_000_000);
        list.push(profiled(&at, "UADA00001", "Ada", &format!("message {i}")));
        real.push(at);
        if i % 7 == 3 {
            list.push(system(&ts(index * 10 + 5, 0), "channel_join"));
        }
    }
    state.history.insert(CH.to_owned(), list);
    (state, real)
}

fn page_request(view: TimelineView, limit: u16) -> TimelineRequest {
    TimelineRequest {
        view,
        thread_id: None,
        before: None,
        after: None,
        limit,
    }
}

fn assert_no_stray_methods(fake: &Fake) {
    fake.with(|state| {
        for (method, _) in &state.requests {
            assert!(
                METHODS.contains(&method.as_str()),
                "the client called {method}, outside the Web API subset"
            );
        }
    });
}

#[tokio::test]
async fn fetch_page_has_discord_semantics_through_a_page_cap_smaller_than_the_limit() {
    let (state, real) = linear(120);
    let fake = Fake::start(state, 20).await;
    let client = fake.client();

    let newest = client
        .fetch_page(&channel(), 50, None, None)
        .await
        .expect("newest");
    assert_eq!(
        ts_list(&newest),
        real[70..],
        "both None is the newest `limit`"
    );
    assert!(
        fake.calls("conversations.history") > 1,
        "a capped page must be followed, never assumed full"
    );

    let older = client
        .fetch_page(&channel(), 50, Some(&newest[0].id), None)
        .await
        .expect("before");
    assert_eq!(
        ts_list(&older),
        real[20..70],
        "before is the NEWEST `limit` strictly older than the cursor"
    );

    let newer = client
        .fetch_page(&channel(), 50, None, Some(&mid(&real[9])))
        .await
        .expect("after");
    assert_eq!(
        ts_list(&newer),
        real[10..60],
        "after is the OLDEST `limit` strictly newer than the cursor"
    );

    let boundary = MessageId::at_time_ms(mid(&real[100]).created_at_ms().expect("dated") - 1);
    let since = client
        .fetch_page(&channel(), 5, None, Some(&boundary))
        .await
        .expect("time boundary");
    assert_eq!(
        ts_list(&since),
        real[100..105],
        "a time boundary works as a cursor"
    );

    assert!(client
        .fetch_page(&channel(), 5, Some(&newest[0].id), Some(&newest[1].id))
        .await
        .is_err());
    assert!(newest.iter().all(|message| message.channel_id == channel()));
    assert!(newest
        .windows(2)
        .all(|pair| pair[0].id.numeric() < pair[1].id.numeric()));
    assert_no_stray_methods(&fake);
}

#[tokio::test]
async fn the_live_poller_walks_forward_across_many_pages_without_skipping_anything() {
    // More messages than one window collection reads, so the first walks must narrow by time.
    let (state, real) = linear(330);
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    let hub = LiveHub::new();
    let mut subscription = hub.subscribe(&channel(), None);
    let mut cursor = Some(mid(&real[4]).numeric().expect("numeric"));
    let mut published = Vec::new();
    for _ in 0..200 {
        let tick = poll_once(&client, &hub, &channel(), 10, &mut cursor)
            .await
            .expect("tick");
        while let Ok(live) = subscription.receiver.try_recv() {
            published.push(live.message);
        }
        if tick.published == 0 && !tick.backlog {
            break;
        }
    }
    assert_eq!(
        ts_list(&published),
        real[5..],
        "every message after the cursor, once each, in order"
    );
    assert!(
        fake.requests("conversations.history")
            .iter()
            .any(|params| params.contains_key("oldest") && params.contains_key("latest")),
        "the walk never needed to narrow, so this test did not exercise the bound"
    );

    // New arrivals are picked up from where the walk stopped.
    let fresh = [ts(5000, 1), ts(5001, 2)];
    fake.with(|state| {
        let list = state.history.get_mut(CH).expect("channel");
        for at in &fresh {
            list.push(plain(at, "UADA00001", "late"));
        }
    });
    poll_once(&client, &hub, &channel(), 10, &mut cursor)
        .await
        .expect("tick");
    let mut late = Vec::new();
    while let Ok(live) = subscription.receiver.try_recv() {
        late.push(live.message);
    }
    assert_eq!(ts_list(&late), fresh);
}

#[tokio::test]
async fn a_forward_walk_through_long_runs_of_system_events_never_jumps_a_message() {
    // Thousands of joins and leaves around the two messages the poller must not jump over: far
    // more than one call may read, so the walk has to make progress across calls.
    let mut state = FakeState::default();
    let mut list: Vec<_> = (0..2600)
        .map(|i| system(&ts(i, 0), "channel_join"))
        .collect();
    list.push(plain(&ts(3000, 0), "UADA00001", "first words"));
    list.extend((0..2400).map(|i| system(&ts(4000 + i, 0), "channel_leave")));
    list.push(plain(&ts(7000, 0), "UADA00001", "second words"));
    list.extend((0..30).map(|i| system(&ts(8000 + i, 0), "channel_join")));
    state.history.insert(CH.to_owned(), list);
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    let mut cursor = mid(&ts(-1, 0));
    let mut walked = Vec::new();
    let mut empty_calls = 0;
    for _ in 0..200 {
        let page = client
            .fetch_page(&channel(), 10, None, Some(&cursor))
            .await
            .expect("after");
        match page.last() {
            Some(last) => cursor = last.id.clone(),
            None => empty_calls += 1,
        }
        walked.extend(page);
        if walked.len() == 2 {
            break;
        }
    }
    assert_eq!(ts_list(&walked), [ts(3000, 0), ts(7000, 0)]);
    assert!(
        empty_calls > 0,
        "the runs were short enough to cross in one call, so resumption was not exercised"
    );
    // Past the last message there is nothing, and saying so is not an error.
    assert!(client
        .fetch_page(&channel(), 10, None, Some(&cursor))
        .await
        .expect("after the end")
        .is_empty());
}

#[tokio::test]
async fn system_events_are_omitted_and_words_are_kept_with_bot_authors_named() {
    let mut state = FakeState::default();
    let mut list = vec![
        profiled(&ts(1, 0), "UADA00001", "Ada", "hello"),
        json!({"type":"message","subtype":"bot_message","ts":ts(2, 0),"bot_id":"B0CIBOT01",
            "username":"ci-bot","text":"build green"}),
        json!({"type":"message","subtype":"file_share","ts":ts(3, 0),"user":"UADA00001",
            "user_profile":{"display_name":"Ada"},"text":"a file"}),
        json!({"type":"message","subtype":"me_message","ts":ts(4, 0),"user":"UADA00001",
            "user_profile":{"display_name":"Ada"},"text":"waves"}),
        json!({"type":"message","ts":ts(5, 0),"user":"UAPPBOT01","bot_id":"B0APPBOT1",
            "bot_profile":{"name":"Deploy App"},"text":"deployed"}),
        json!({"type":"message","subtype":"thread_broadcast","ts":ts(6, 0),"thread_ts":ts(1, 0),
            "user":"UADA00001","user_profile":{"display_name":"Ada"},"text":"also sent to channel"}),
    ];
    for (offset, subtype) in [
        "channel_join",
        "channel_leave",
        "channel_topic",
        "channel_purpose",
        "channel_name",
        "group_join",
        "tombstone",
        "pinned_item",
    ]
    .into_iter()
    .enumerate()
    {
        list.push(system(
            &ts(10 + i64::try_from(offset).expect("small"), 0),
            subtype,
        ));
    }
    state.history.insert(CH.to_owned(), list);
    let fake = Fake::start(state, 3).await;
    let client = fake.client();
    let messages = client.fetch_recent(&channel(), 50).await.expect("read");
    assert_eq!(
        messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        [
            "hello",
            "build green",
            "a file",
            "waves",
            "deployed",
            "also sent to channel"
        ]
    );
    let ci = &messages[1];
    assert_eq!(
        (ci.author.as_str(), ci.author_id.as_str(), ci.author_is_bot),
        ("ci-bot", "B0CIBOT01", true)
    );
    let app = &messages[4];
    assert_eq!(
        (
            app.author.as_str(),
            app.author_id.as_str(),
            app.author_is_bot
        ),
        ("Deploy App", "UAPPBOT01", true),
        "an app's bot user keeps its mentionable user id"
    );
    assert!(!messages[0].author_is_bot);
    assert_eq!(messages[0].timestamp, "2023-11-14T22:13:21.000000Z");
    assert!(messages[0].spoken_time.is_empty() && messages[0].spoken_content.is_empty());
    assert_eq!(messages[0].reply_to, None);
    let broadcast = messages[5].thread.as_ref().expect("broadcast membership");
    assert_eq!(broadcast.id, ts(1, 0));
    assert!(!broadcast.is_root);
    assert_eq!(broadcast.root_message_id, Some(mid(&ts(1, 0))));
    assert_eq!(
        fake.calls("users.info"),
        0,
        "authors who carry a profile need no lookup"
    );
}

#[tokio::test]
async fn author_lookups_are_cached_and_a_failed_lookup_degrades_to_the_raw_id() {
    let mut state = FakeState::default();
    state.history.insert(
        CH.to_owned(),
        vec![
            plain(&ts(1, 0), "UADA00001", "one"),
            plain(&ts(2, 0), "UROBOT001", "two"),
            plain(&ts(3, 0), "UGONE0001", "three"),
            plain(&ts(4, 0), "UADA00001", "four"),
        ],
    );
    state.users.insert(
        "UADA00001".to_owned(),
        json!({"id":"UADA00001","name":"ada","real_name":"Ada L",
            "profile":{"display_name":"Ada Lovelace","real_name":"Ada L"}}),
    );
    state.users.insert(
        "UROBOT001".to_owned(),
        json!({"id":"UROBOT001","name":"robot","is_bot":true,"profile":{"display_name":""}}),
    );
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    for _ in 0..3 {
        let messages = client.fetch_recent(&channel(), 10).await.expect("read");
        assert_eq!(
            messages
                .iter()
                .map(|message| (message.author.as_str(), message.author_is_bot))
                .collect::<Vec<_>>(),
            [
                ("Ada Lovelace", false),
                ("robot", true),
                ("UGONE0001", false),
                ("Ada Lovelace", false)
            ]
        );
    }
    assert_eq!(
        fake.calls("users.info"),
        3,
        "one lookup per author across three reads, the failed one included"
    );
}

/// A channel whose root `ts(100, 0)` has `count` replies.
fn threaded_fixture(count: usize) -> (FakeState, String, Vec<String>) {
    let mut state = FakeState::default();
    let root_ts = ts(100, 0);
    let mut root = profiled(&root_ts, "UADA00001", "Ada", "the linked conversation");
    let replies: Vec<(String, String)> = (0..count)
        .map(|i| {
            let index = i64::try_from(i).expect("small");
            (ts(200 + index, index), format!("reply {i}"))
        })
        .collect();
    thread(&mut state, CH, &mut root, &replies);
    state.history.insert(
        CH.to_owned(),
        vec![
            profiled(&ts(50, 0), "UADA00001", "Ada", "before"),
            root,
            profiled(&ts(150, 0), "UADA00001", "Ada", "after"),
        ],
    );
    let mut all = vec![root_ts.clone()];
    all.extend(replies.into_iter().map(|(at, _)| at));
    (state, root_ts, all)
}

#[tokio::test]
async fn a_channel_narrowed_to_one_thread_reads_posts_and_pages_inside_that_thread() {
    for root_always in [false, true] {
        let (mut state, root_ts, all) = threaded_fixture(45);
        state.root_always = root_always;
        let fake = Fake::start(state, 20).await;
        let client = fake.client();
        let link = format!(
            "https://example.slack.com/archives/{CH}/p{}",
            root_ts.replace('.', "")
        );
        let registered = client
            .register_channel(&link, "release thread")
            .await
            .expect("register");
        assert_eq!(registered.id.as_str(), format!("{CH}~{root_ts}"));
        assert!(!registered.created, "nothing upstream was created");
        assert!(registered.writable);
        let scoped = registered.id;

        let everything = client
            .fetch_page(&scoped, 100, None, None)
            .await
            .expect("whole thread");
        assert_eq!(ts_list(&everything), all, "root first, then every reply");
        assert!(everything[0].thread.as_ref().expect("root").is_root);
        assert_eq!(
            everything[0].thread.as_ref().expect("root").reply_count,
            Some(45)
        );
        assert!(everything
            .iter()
            .all(|message| message.channel_id == scoped));

        let recent = client.fetch_recent(&scoped, 10).await.expect("recent");
        assert_eq!(ts_list(&recent), all[36..]);
        let older = client
            .fetch_page(&scoped, 10, Some(&recent[0].id), None)
            .await
            .expect("before");
        assert_eq!(ts_list(&older), all[26..36]);

        // Walk forward from the root, as the live poller would.
        let mut cursor = mid(&root_ts);
        let mut walked = Vec::new();
        loop {
            let page = client
                .fetch_page(&scoped, 7, None, Some(&cursor))
                .await
                .expect("after");
            let Some(last) = page.last() else { break };
            cursor = last.id.clone();
            walked.extend(page);
        }
        assert_eq!(ts_list(&walked), all[1..], "root_always={root_always}");

        let mut request = page_request(TimelineView::Main, 20);
        let mut pages = Vec::new();
        loop {
            let page = client
                .fetch_timeline(&scoped, &request)
                .await
                .expect("main");
            assert!(!page.has_threads, "a thread-scoped channel has no children");
            assert!(page.threads.is_empty());
            let scope = page.thread.as_ref().expect("names its thread");
            assert_eq!(scope.id, root_ts);
            assert_eq!(scope.reply_count, Some(45));
            assert!(scope.reply_count_exact);
            assert_eq!(scope.title, "the linked conversation");
            pages.insert(0, ts_list(&page.messages));
            request.before = page.next_before;
            if request.before.is_none() {
                break;
            }
        }
        assert_eq!(
            pages.concat(),
            all,
            "Main pages back through root and replies"
        );

        let threads = client
            .fetch_timeline(&scoped, &page_request(TimelineView::Threads, 20))
            .await
            .expect("threads");
        assert!(threads.threads.is_empty() && !threads.has_threads);
        assert_eq!(threads.thread.expect("scope").id, root_ts);

        let wrong = TimelineRequest {
            thread_id: Some(ts(150, 0)),
            ..page_request(TimelineView::Thread, 20)
        };
        assert!(matches!(
            client
                .fetch_timeline(&scoped, &wrong)
                .await
                .expect_err("other thread")
                .cause(),
            ChatError::Refused(_)
        ));

        let posted = client
            .post_message(&scoped, "on it", Some(&mid(&all[3])))
            .await
            .expect("post");
        let sent = fake.requests("chat.postMessage");
        assert_eq!(sent[0]["thread_ts"], root_ts, "posts go into the thread");
        assert_eq!(sent[0]["channel"], CH);
        assert_eq!(posted.channel_id, scoped);
        assert_eq!(posted.thread.as_ref().expect("membership").id, root_ts);
        assert!(client
            .post_in_thread(&scoped, &ts(150, 0), "elsewhere", None)
            .await
            .is_err());

        let report = vibe_talk::probe::probe_channels(
            &client,
            &[ChannelInfo {
                id: scoped.clone(),
                label: "release thread".to_owned(),
                writable: true,
                alias: None,
                added: true,
                provider: Some("slack".to_owned()),
            }],
        )
        .await;
        assert!(!report.is_failure(), "{}", report.render());
        assert_no_stray_methods(&fake);
    }
}

/// A conversation with three threads of different sizes and activity.
fn three_threads() -> (FakeState, [String; 3]) {
    let mut state = FakeState::default();
    let long_title = format!("{}\nsecond line", "word ".repeat(30));
    let (r1, r2, r3) = (ts(10, 0), ts(20, 0), ts(30, 0));
    let mut root1 = profiled(&r1, "UADA00001", "Ada", &long_title);
    let mut root2 = profiled(&r2, "UADA00001", "Ada", "  busy thread  \nmore");
    let mut root3 = profiled(&r3, "UADA00001", "Ada", "");
    // Activity order: r2 (500) < r1 (1000) < r3 (2000), unlike creation order.
    thread(
        &mut state,
        CH,
        &mut root1,
        &[(ts(900, 0), "a".into()), (ts(1000, 0), "b".into())],
    );
    let busy: Vec<(String, String)> = (0..60)
        .map(|i| (ts(400 + i, 0), format!("busy {i}")))
        .collect();
    let mut busy = busy;
    busy.last_mut().expect("reply").0 = ts(500, 0);
    busy.sort();
    busy.dedup_by(|a, b| a.0 == b.0);
    thread(&mut state, CH, &mut root2, &busy);
    thread(&mut state, CH, &mut root3, &[(ts(2000, 0), "late".into())]);
    let mut list = vec![root1, root2, root3];
    for i in 0..30 {
        list.push(profiled(
            &ts(40 + i, 0),
            "UADA00001",
            "Ada",
            &format!("chat {i}"),
        ));
    }
    // A broadcast reply shows in the channel and in its thread.
    let broadcast = json!({"type":"message","subtype":"thread_broadcast","ts":ts(1000, 0),
        "thread_ts":r1,"user":"UREPLIER1","user_profile":{"display_name":"Replier"},"text":"b"});
    list.push(broadcast);
    state.history.insert(CH.to_owned(), list);
    (state, [r1, r2, r3])
}

#[tokio::test]
async fn the_main_view_pages_back_through_the_whole_conversation() {
    let (state, _) = three_threads();
    let fake = Fake::start(state, 4).await;
    let client = fake.client();
    let expected = ts_list(&client.fetch_recent(&channel(), 1000).await.expect("all"));
    assert_eq!(expected.len(), 34);
    let mut request = page_request(TimelineView::Main, 7);
    let mut pages = Vec::new();
    loop {
        let page = client
            .fetch_timeline(&channel(), &request)
            .await
            .expect("main page");
        assert!(page.has_threads);
        assert!(page.thread.is_none());
        assert_eq!(page.has_more, page.next_before.is_some());
        assert!(page.messages.len() <= 7);
        pages.insert(0, ts_list(&page.messages));
        request.before = page.next_before;
        if request.before.is_none() {
            break;
        }
    }
    assert_eq!(pages.concat(), expected);

    // A cursor is bound to the view it was issued for.
    let first = client
        .fetch_timeline(&channel(), &page_request(TimelineView::Main, 7))
        .await
        .expect("main");
    let crossed = TimelineRequest {
        before: first.next_before,
        ..page_request(TimelineView::Threads, 7)
    };
    assert!(matches!(
        client
            .fetch_timeline(&channel(), &crossed)
            .await
            .expect_err("cursor from another view")
            .cause(),
        ChatError::Refused(_)
    ));
}

#[tokio::test]
async fn the_thread_list_orders_by_latest_reply_and_pages_with_a_bound_cursor() {
    let (state, [r1, r2, r3]) = three_threads();
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    let mut request = page_request(TimelineView::Threads, 2);
    let first = client
        .fetch_timeline(&channel(), &request)
        .await
        .expect("threads");
    assert!(first.has_threads && first.has_more && first.messages.is_empty());
    assert_eq!(first.notice, None, "the whole history was scanned");
    assert_eq!(
        first
            .threads
            .iter()
            .map(|summary| summary.id.clone())
            .collect::<Vec<_>>(),
        [r1.clone(), r3.clone()],
        "newest activity last, oldest first within the page"
    );
    let title = &first.threads[0].title;
    assert!(
        title.chars().count() <= 80 && title.ends_with('…'),
        "{title:?}"
    );
    assert!(!title.contains("second line"));
    assert_eq!(
        first.threads[1].title, "Thread",
        "an empty root has a neutral title"
    );
    assert_eq!(first.threads[0].updated_at, "2023-11-14T22:30:00.000000Z");
    assert_eq!(first.threads[0].reply_count, Some(2));
    assert!(first.threads[0].reply_count_exact);
    assert_eq!(first.threads[0].root.as_ref().expect("root").id, mid(&r1));
    request.before = first.next_before;
    let second = client
        .fetch_timeline(&channel(), &request)
        .await
        .expect("older threads");
    assert_eq!(second.threads.len(), 1);
    assert_eq!(second.threads[0].id, r2);
    assert_eq!(second.threads[0].title, "busy thread");
    assert!(!second.has_more && second.next_before.is_none());
    assert!(client
        .fetch_timeline(&ChannelId("C0000OTHER".to_owned()), &request)
        .await
        .is_err());
}

#[tokio::test]
async fn the_thread_view_verifies_membership_and_pages_back_to_the_root() {
    let (state, [_, r2, _]) = three_threads();
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    let mut request = TimelineRequest {
        thread_id: Some(r2.clone()),
        ..page_request(TimelineView::Thread, 25)
    };
    let mut pages = Vec::new();
    loop {
        let page = client
            .fetch_timeline(&channel(), &request)
            .await
            .expect("thread page");
        assert!(page.has_threads);
        let selected = page.thread.as_ref().expect("selected thread");
        assert_eq!(selected.id, r2);
        assert_eq!(selected.reply_count, Some(60));
        assert!(page.messages.iter().all(|message| message
            .thread
            .as_ref()
            .expect("membership")
            .id
            == r2));
        pages.insert(0, page.messages);
        request.before = page.next_before;
        if request.before.is_none() {
            break;
        }
    }
    assert_eq!(pages.len(), 3);
    let all: Vec<Message> = pages.concat();
    assert_eq!(all.len(), 61);
    assert!(all[0].thread.as_ref().expect("root").is_root);
    assert!(all[1..]
        .iter()
        .all(|message| !message.thread.as_ref().expect("reply").is_root));

    let missing = TimelineRequest {
        thread_id: Some(ts(77_777, 0)),
        ..page_request(TimelineView::Thread, 25)
    };
    let error = client
        .fetch_timeline(&channel(), &missing)
        .await
        .expect_err("not a thread here");
    assert!(
        matches!(error.cause(), ChatError::Status { status: 404, body } if body.contains("thread_not_found")),
        "{error}"
    );
    assert_eq!(classify(&error), Diagnosis::UnknownChannel);
    let not_canonical = TimelineRequest {
        thread_id: Some("1700000020.0".to_owned()),
        ..page_request(TimelineView::Thread, 25)
    };
    assert!(client
        .fetch_timeline(&channel(), &not_canonical)
        .await
        .is_err());
}

#[tokio::test]
async fn the_flat_view_merges_replies_of_the_roots_on_the_page_and_states_its_bound() {
    let (state, [r1, r2, r3]) = three_threads();
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    let page = client
        .fetch_timeline(&channel(), &page_request(TimelineView::Flat, 50))
        .await
        .expect("flat");
    let got = ts_list(&page.messages);
    assert!(got.windows(2).all(|pair| pair[0] < pair[1]), "oldest first");
    for root in [&r1, &r2, &r3] {
        assert!(got.contains(root));
    }
    assert!(got.contains(&ts(900, 0)) && got.contains(&ts(2000, 0)));
    let busy = page
        .messages
        .iter()
        .filter(|message| {
            message
                .thread
                .as_ref()
                .is_some_and(|thread| thread.id == r2 && !thread.is_root)
        })
        .count();
    assert_eq!(busy, 50, "the newest 50 replies of the busy thread");
    assert_eq!(
        got.iter().filter(|at| **at == ts(1000, 0)).count(),
        1,
        "a broadcast reply appears once"
    );
    let notice = page.notice.expect("the busy thread exceeded the bound");
    assert!(notice.contains("10") && notice.contains("50"), "{notice}");

    let (state, _, _) = threaded_fixture(3);
    let small = Fake::start(state, 20).await;
    let quiet = small
        .client()
        .fetch_timeline(&channel(), &page_request(TimelineView::Flat, 50))
        .await
        .expect("small flat");
    assert_eq!(quiet.messages.len(), 6);
    assert_eq!(quiet.notice, None, "no bound applied, so none is stated");
}

#[tokio::test]
async fn posting_escapes_room_pings_and_a_reply_goes_into_the_targets_thread() {
    let (state, [r1, _, _]) = three_threads();
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    assert_eq!(client.self_author_id(), None);

    let posted = client
        .post_message(&channel(), "deploy & <!channel> <@UADA00001>", None)
        .await
        .expect("post");
    let sent = fake.requests("chat.postMessage");
    assert_eq!(
        sent[0]["text"],
        "deploy &amp; &lt;!channel&gt; <@UADA00001>"
    );
    assert!(!sent[0].contains_key("thread_ts"));
    assert_eq!(posted.content, "deploy & <!channel> <@UADA00001>");
    assert_eq!(posted.channel_id, channel());
    assert_eq!(posted.author_id.as_str(), SELF_USER);
    assert!(posted.author_is_bot);
    assert_eq!(
        client.self_author_id().as_deref(),
        Some(SELF_USER),
        "a post teaches the client which account it posts as"
    );

    client
        .post_message(&channel(), "ack", Some(&mid(&r1)))
        .await
        .expect("reply to a root");
    client
        .post_message(&channel(), "ack", Some(&mid(&ts(1000, 0))))
        .await
        .expect("reply to a broadcast");
    let plain_target = ts(40, 0);
    client
        .post_message(&channel(), "ack", Some(&mid(&plain_target)))
        .await
        .expect("reply starts a thread");
    let sent = fake.requests("chat.postMessage");
    assert_eq!(sent[1]["thread_ts"], r1);
    assert_eq!(
        sent[2]["thread_ts"], r1,
        "a broadcast's thread is its parent's"
    );
    assert_eq!(sent[3]["thread_ts"], plain_target);

    for (content, reply_to) in [
        ("   ", None),
        (&*"x".repeat(40_001), None),
        ("ack", Some(mid(&ts(901, 0)))),
    ] {
        let error = client
            .post_message(&channel(), content, reply_to.as_ref())
            .await
            .expect_err("refused");
        assert!(matches!(error.cause(), ChatError::Refused(_)), "{error}");
    }
    assert_eq!(fake.calls("chat.postMessage"), 4, "a refusal sends nothing");
}

#[tokio::test]
async fn thread_posts_and_lookups_verify_the_thread_first() {
    let (state, [r1, _, r3]) = three_threads();
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    let posted = client
        .post_in_thread(&channel(), &r1, "in the thread", Some(&mid(&r3)))
        .await
        .expect("thread post");
    assert_eq!(posted.thread.as_ref().expect("membership").id, r1);
    assert_eq!(fake.requests("chat.postMessage")[0]["thread_ts"], r1);

    let error = client
        .post_in_thread(&channel(), &ts(77_777, 0), "nowhere", None)
        .await
        .expect_err("unknown thread");
    assert!(matches!(
        error.cause(),
        ChatError::Status { status: 404, .. }
    ));
    assert_eq!(fake.calls("chat.postMessage"), 1, "verified before posting");

    let reply = client
        .fetch_thread_message(&channel(), &r1, &mid(&ts(900, 0)))
        .await
        .expect("reply");
    assert_eq!(reply.content, "a");
    let root = client
        .fetch_thread_message(&channel(), &r1, &mid(&r1))
        .await
        .expect("root");
    assert!(root.thread.expect("root membership").is_root);
    assert!(client
        .fetch_thread_message(&channel(), &r3, &mid(&ts(900, 0)))
        .await
        .is_err());
}

/// `#252 thread-button-state`. Slack lets any message on a conversation's own timeline head a
/// thread, and writes `thread_ts` only once somebody has answered. So a message nobody has answered
/// is named as the root of its own thread, by its `ts`, with an exact count of nothing — as every
/// message nobody has answered on Google Chat is — and that thread can be opened and posted into
/// before it has a reply: the first post is what starts it.
#[tokio::test]
async fn a_message_nobody_has_answered_heads_its_own_empty_thread_which_its_first_post_starts() {
    let (mut state, [r1, _, _]) = three_threads();
    let fresh = ts(3000, 250);
    state
        .history
        .get_mut(CH)
        .expect("the conversation")
        .push(profiled(
            &fresh,
            "UADA00001",
            "Ada",
            "nobody has answered this",
        ));
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    assert!(
        client.replies_in_thread(),
        "a Slack reply is a thread reply"
    );

    let messages = client.fetch_recent(&channel(), 50).await.expect("read");
    let lone = messages
        .iter()
        .find(|message| message.id == mid(&fresh))
        .expect("the new message");
    assert_eq!(
        lone.thread,
        Some(MessageThread {
            id: fresh.clone(),
            root_message_id: Some(mid(&fresh)),
            is_root: true,
            reply_count: Some(0),
            reply_count_exact: true,
        }),
        "a message nobody has answered names no thread to start"
    );
    let answered = messages
        .iter()
        .find(|message| message.id == mid(&r1))
        .and_then(|message| message.thread.as_ref())
        .expect("an answered root");
    assert_eq!(
        (answered.id.as_str(), answered.is_root, answered.reply_count),
        (r1.as_str(), true, Some(2))
    );
    // Every message on the conversation's own timeline heads a thread or is in one.
    assert!(messages.iter().all(|message| message.thread.is_some()));

    // Its thread is the message alone, with no reply yet, and opens without one.
    let request = TimelineRequest {
        thread_id: Some(fresh.clone()),
        ..page_request(TimelineView::Thread, 25)
    };
    let page = client
        .fetch_timeline(&channel(), &request)
        .await
        .expect("the empty thread");
    assert_eq!(ts_list(&page.messages), std::slice::from_ref(&fresh));
    assert_eq!(page.messages[0].thread, lone.thread);
    let summary = page.thread.expect("the thread is named");
    assert_eq!(
        (summary.id.as_str(), summary.reply_count),
        (fresh.as_str(), Some(0))
    );

    // The first post into it goes into the message's thread, and is a reply in it.
    let posted = client
        .post_in_thread(&channel(), &fresh, "the first reply", None)
        .await
        .expect("the first reply");
    assert_eq!(fake.requests("chat.postMessage")[0]["thread_ts"], fresh);
    let membership = posted.thread.expect("the reply's thread");
    assert_eq!(
        (
            membership.id.as_str(),
            membership.is_root,
            membership.root_message_id
        ),
        (fresh.as_str(), false, Some(mid(&fresh)))
    );
    let page = client
        .fetch_timeline(&channel(), &request)
        .await
        .expect("the thread, answered");
    assert_eq!(
        ts_list(&page.messages),
        [fresh.clone(), ts_from_message_id(&posted.id).expect("ts")]
    );
}

#[tokio::test]
async fn registration_accepts_every_link_shape_and_verifies_access() {
    let (state, root_ts, _) = threaded_fixture(2);
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    let scoped = format!("{CH}~{root_ts}");
    let packed = root_ts.replace('.', "");
    for (source, expected) in [
        (CH.to_owned(), CH.to_owned()),
        (scoped.clone(), scoped.clone()),
        (
            format!("https://example.slack.com/archives/{CH}"),
            CH.to_owned(),
        ),
        (
            format!("https://example.slack.com/archives/{CH}/p{packed}"),
            scoped.clone(),
        ),
        (
            format!(
                "https://example.slack.com/archives/{CH}/p1700000201000001?thread_ts={root_ts}&cid={CH}"
            ),
            scoped.clone(),
        ),
        (
            format!("https://app.slack.com/client/T0000TEAM/{CH}"),
            CH.to_owned(),
        ),
        (
            format!("https://app.slack.com/client/T0000TEAM/{CH}/thread/{CH}-{root_ts}"),
            scoped.clone(),
        ),
    ] {
        let registered = client
            .register_channel(&source, "label")
            .await
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert_eq!(registered.id.as_str(), expected, "{source}");
        assert!(!registered.created && registered.writable);
    }
    assert_eq!(fake.calls("conversations.info"), 7);

    let unknown = client
        .register_channel("C0000NOPE1", "label")
        .await
        .expect_err("no such conversation");
    assert!(matches!(
        unknown.cause(),
        ChatError::Status { status: 404, .. }
    ));
    let no_thread = client
        .register_channel(&format!("{CH}~{}", ts(77_777, 0)), "label")
        .await
        .expect_err("no such thread");
    assert!(matches!(
        no_thread.cause(),
        ChatError::Status { status: 404, .. }
    ));
    let unparseable = client
        .register_channel("https://example.slack.com/huddle/T1/C2", "label")
        .await
        .expect_err("not a conversation link");
    assert!(
        matches!(unparseable.cause(), ChatError::Refused(detail) if detail.contains("archives"))
    );
    client
        .unregister_channel(&ChannelId(scoped))
        .await
        .expect("nothing upstream to remove");

    let mut config = fake.config();
    config.channel_registration = false;
    config.registered_channels_writable = false;
    let disabled = HttpSlackClient::new(&config).expect("client");
    assert!(matches!(
        disabled
            .register_channel(CH, "label")
            .await
            .expect_err("disabled")
            .cause(),
        ChatError::Refused(_)
    ));
}

fn directory_fixture() -> FakeState {
    let mut state = FakeState::default();
    let mut memberships = Vec::new();
    for i in 0..10 {
        memberships.push(
            json!({"id": format!("C00000CH{i}"), "name": format!("team-{i}"),
            "is_private": false}),
        );
    }
    memberships.push(json!({"id":"C0000OPS01","name":"eng-ops"}));
    memberships.push(json!({"id":"G0000OPS02","name":"ops-private","is_private":true}));
    memberships.push(json!({"id":"D0000ADA01","is_im":true,"user":"UADA00001"}));
    memberships.push(json!({"id":"D0000GHX01","is_im":true,"user":"UGHOST001"}));
    memberships.push(json!({"id":"G0000MPIM1","is_mpim":true,"name":"mpdm-ada--grace--alan-1"}));
    for i in 0..6 {
        memberships.push(json!({"id": format!("C00000OP{i}"), "name": format!("ops-{i}")}));
    }
    state.memberships = memberships;
    state.users.insert(
        "UADA00001".to_owned(),
        json!({"id":"UADA00001","name":"ada","profile":{"display_name":"","real_name":"Ada Lovelace"}}),
    );
    state
}

#[tokio::test]
async fn discovery_names_conversations_filters_and_never_overflows_a_page() {
    let fake = Fake::start(directory_fixture(), 4).await;
    let client = fake.client();
    let mut request = DirectoryRequest {
        query: None,
        cursor: None,
        limit: 5,
    };
    let mut names = Vec::new();
    loop {
        let page = client.discover_channels(&request).await.expect("page");
        assert!(page.entries.len() <= 5);
        for entry in &page.entries {
            assert!(entry.registered_channel_id.is_none());
            assert!(vibe_talk::slack::ids::is_conversation_id(&entry.source));
        }
        names.extend(
            page.entries
                .into_iter()
                .map(|entry| (entry.source, entry.name)),
        );
        request.cursor = page.next_cursor;
        if request.cursor.is_none() {
            break;
        }
    }
    assert_eq!(names.len(), 21, "every membership exactly once: {names:?}");
    let named = |id: &str| {
        names
            .iter()
            .find(|(source, _)| source == id)
            .map(|(_, name)| name.as_str())
    };
    assert_eq!(named("C00000CH0"), Some("#team-0"));
    assert_eq!(named("D0000ADA01"), Some("DM: Ada Lovelace"));
    assert_eq!(named("D0000GHX01"), Some("DM: UGHOST001"));
    assert_eq!(named("G0000MPIM1"), Some("Group: ada, grace, alan"));

    let mut request = DirectoryRequest {
        query: Some("OPS".to_owned()),
        cursor: None,
        limit: 3,
    };
    let mut matched = Vec::new();
    loop {
        let page = client.discover_channels(&request).await.expect("page");
        assert!(page.entries.len() <= 3);
        matched.extend(page.entries.into_iter().map(|entry| entry.name));
        request.cursor = page.next_cursor;
        if request.cursor.is_none() {
            break;
        }
    }
    let mut expected = vec!["#eng-ops".to_owned(), "#ops-private".to_owned()];
    expected.extend((0..6).map(|i| format!("#ops-{i}")));
    assert_eq!(
        matched, expected,
        "case-insensitive, in order, none repeated"
    );

    let mut config = fake.config();
    config.channel_discovery = false;
    let disabled = HttpSlackClient::new(&config).expect("client");
    assert!(disabled
        .discover_channels(&DirectoryRequest {
            query: None,
            cursor: None,
            limit: 5
        })
        .await
        .is_err());
    assert_no_stray_methods(&fake);
}

#[tokio::test]
async fn identity_comes_from_auth_test_and_is_remembered() {
    let (state, _) = linear(1);
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    assert_eq!(client.self_author_id(), None);
    let identity = client.identity().await.expect("identity");
    assert_eq!(identity.id, SELF_USER);
    assert_eq!(identity.username, "vibe-bot");
    assert_eq!(client.self_author_id().as_deref(), Some(SELF_USER));
    assert_eq!(client.owner_author_id().as_deref(), Some("UOWNER001"));
    assert_eq!(client.provider_name(), "Slack");
}

#[tokio::test]
async fn slack_errors_map_to_statuses_the_probe_classifies() {
    for (method, error, status, diagnosis) in [
        ("auth.test", "invalid_auth", 401, Diagnosis::InvalidToken),
        ("auth.test", "token_revoked", 401, Diagnosis::InvalidToken),
        (
            "conversations.history",
            "missing_scope",
            403,
            Diagnosis::NoAccess,
        ),
        (
            "conversations.history",
            "not_in_channel",
            403,
            Diagnosis::NoAccess,
        ),
        (
            "conversations.history",
            "channel_not_found",
            404,
            Diagnosis::UnknownChannel,
        ),
    ] {
        let (mut state, _) = linear(3);
        state.fail.insert(method.to_owned(), error.to_owned());
        let fake = Fake::start(state, 20).await;
        let client = fake.client();
        let failure = if method == "auth.test" {
            client.identity().await.expect_err("fails")
        } else {
            client.fetch_recent(&channel(), 5).await.expect_err("fails")
        };
        let rendered = failure.to_string();
        assert!(
            matches!(failure.cause(), ChatError::Status { status: got, body } if *got == status && body.contains(error)),
            "{rendered}"
        );
        assert_eq!(classify(&failure), diagnosis, "{error}");
        assert!(rendered.starts_with("Slack returned HTTP"), "{rendered}");
        assert!(!rendered.contains(TOKEN));
        if error == "missing_scope" {
            assert!(
                rendered.contains("needed scope: channels:history"),
                "{rendered}"
            );
        }
    }

    let (mut state, _) = linear(3);
    state
        .fail
        .insert("conversations.history".to_owned(), "fatal_error".to_owned());
    let fake = Fake::start(state, 20).await;
    let failure = fake
        .client()
        .fetch_recent(&channel(), 5)
        .await
        .expect_err("fails");
    assert!(
        matches!(failure.cause(), ChatError::Status { body, .. } if body.contains("conversations.history answered fatal_error")),
        "{failure}"
    );

    let mut config = fake.config();
    config.token = vibe_talk::config::Secret::new("xoxb-wrong-token");
    let wrong = HttpSlackClient::new(&config).expect("client");
    assert_eq!(
        classify(&wrong.identity().await.expect_err("bad token")),
        Diagnosis::InvalidToken
    );
}

#[tokio::test]
async fn a_rate_limit_is_waited_out_within_budget_and_reported_beyond_it() {
    let (mut state, real) = linear(3);
    state.rate_limit_once = Some(("conversations.history".to_owned(), 1));
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    let started = Instant::now();
    let messages = client.fetch_recent(&channel(), 5).await.expect("retried");
    assert!(
        started.elapsed() >= Duration::from_millis(950),
        "Retry-After was not honoured: {:?}",
        started.elapsed()
    );
    assert_eq!(ts_list(&messages), real);
    assert_eq!(fake.calls("conversations.history"), 2);

    fake.with(|state| state.rate_limit_once = Some(("conversations.history".to_owned(), 120)));
    let started = Instant::now();
    let error = client
        .fetch_recent(&channel(), 5)
        .await
        .expect_err("beyond the budget");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a wait longer than the budget must be refused before it is taken"
    );
    assert!(matches!(error.cause(), ChatError::RateLimited(limit)
        if limit.provider == "slack" && limit.route == "conversations.history" && limit.attempts == 1));
    assert_eq!(error.retry_after(), Some(Duration::from_secs(120)));
    assert_eq!(classify(&error), Diagnosis::RateLimited);
    let rendered = error.to_string();
    assert!(rendered.starts_with("Slack RATE LIMIT"), "{rendered}");

    // An author lookup never waits: the name degrades to the id instead.
    let mut state = FakeState::default();
    state
        .history
        .insert(CH.to_owned(), vec![plain(&ts(1, 0), "UADA00001", "hi")]);
    state.users.insert(
        "UADA00001".to_owned(),
        json!({"id":"UADA00001","name":"ada"}),
    );
    state.rate_limit_once = Some(("users.info".to_owned(), 20));
    let fake = Fake::start(state, 20).await;
    let started = Instant::now();
    let messages = fake
        .client()
        .fetch_recent(&channel(), 5)
        .await
        .expect("read");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(messages[0].author, "UADA00001");
}

#[tokio::test]
async fn the_token_never_appears_in_an_error_or_a_debug_rendering() {
    let (mut state, _) = linear(3);
    state.echo_token_500 = true;
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    let error = client
        .fetch_recent(&channel(), 5)
        .await
        .expect_err("server error");
    let rendered = error.to_string();
    assert!(rendered.contains("HTTP 500"), "{rendered}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(!rendered.contains(TOKEN), "token leaked: {rendered}");
    assert!(!format!("{error:?}").contains(TOKEN));
    assert!(!format!("{client:?}").contains(TOKEN));
    assert!(!format!("{:?}", fake.config()).contains(TOKEN));
    for failure in [
        client.identity().await.expect_err("fails"),
        client
            .register_channel(CH, "label")
            .await
            .expect_err("fails"),
        client
            .post_message(&channel(), "hello", None)
            .await
            .expect_err("fails"),
    ] {
        assert!(!failure.to_string().contains(TOKEN));
    }
}

#[tokio::test]
async fn an_id_from_another_provider_is_refused_before_any_request() {
    let (state, _) = linear(3);
    let fake = Fake::start(state, 20).await;
    let client = fake.client();
    for bad in ["1532416065114607829", "general", "C0000MAIN1~nope"] {
        let error = client
            .fetch_recent(&ChannelId(bad.to_owned()), 5)
            .await
            .expect_err("not a Slack channel");
        assert!(matches!(error.cause(), ChatError::Refused(_)), "{bad}");
    }
    let foreign = MessageId(((1_u64 << 22) | 1000).to_string());
    assert!(client
        .fetch_page(&channel(), 5, Some(&foreign), None)
        .await
        .is_err());
    assert_eq!(fake.calls("conversations.history"), 0);
}

/// `#203 incremental-refresh`. A server that repeats a thread's root on every replies page must
/// not make a thread delta stop short. A delta asks for one more reply than it returns to learn
/// whether more remain; were the root counted in that, three new replies on pages of two would come
/// back as two and no `more`. The page reader drops anything outside its bounds, the root included,
/// before a caller counts, and this pins that it stays so.
#[tokio::test]
async fn a_thread_delta_with_more_replies_than_a_page_says_so_when_the_root_is_repeated() {
    let (mut state, [_, r2, _]) = three_threads();
    state.root_always = true;
    let fake = Fake::start(state, 4).await;
    let client = fake.client();
    let thread = TimelineRequest {
        thread_id: Some(r2.clone()),
        ..page_request(TimelineView::Thread, 25)
    };
    let newest = client
        .fetch_timeline(&channel(), &thread)
        .await
        .expect("thread");
    fake.with(|state| {
        let replies = state
            .replies
            .get_mut(&(CH.to_owned(), r2.clone()))
            .expect("replies");
        for second in 5000..5003 {
            replies.push(
                json!({"type": "message", "ts": ts(second, 0), "thread_ts": r2,
                "user": "UREPLIER1", "text": format!("reply {second}"),
                "user_profile": {"display_name": "Replier"}}),
            );
        }
    });
    let two = TimelineRequest {
        thread_id: Some(r2.clone()),
        ..page_request(TimelineView::Thread, 2)
    };
    let first = client
        .fetch_timeline(
            &channel(),
            &TimelineRequest {
                after: newest.next_after.clone(),
                ..two.clone()
            },
        )
        .await
        .expect("thread delta");
    assert_eq!(ts_list(&first.messages), [ts(5000, 0), ts(5001, 0)]);
    assert!(
        first.delta.as_ref().expect("a delta").more,
        "a third new reply waited for the next refresh"
    );
    let rest = client
        .fetch_timeline(
            &channel(),
            &TimelineRequest {
                after: first.next_after.clone(),
                ..two
            },
        )
        .await
        .expect("the rest");
    assert_eq!(ts_list(&rest.messages), [ts(5002, 0)]);
    assert!(!rest.delta.expect("a delta").more);
}

/// `#203 incremental-refresh`. A conversation's Main, and one thread, are read forward with
/// `oldest=`: only what was posted after the newest message delivered, oldest first, and said to
/// carry additions only. The thread list and Flat issue no native cursor.
#[tokio::test]
async fn main_and_a_thread_are_read_forward_from_the_newest_ts_delivered() {
    let (state, [_, r2, _]) = three_threads();
    let fake = Fake::start(state, 4).await;
    let client = fake.client();
    let main = page_request(TimelineView::Main, 7);
    let newest = client
        .fetch_timeline(&channel(), &main)
        .await
        .expect("main");
    let cursor = newest.next_after.expect("Main is read forward natively");
    fake.with(|state| {
        let list = state.history.get_mut(CH).expect("history");
        list.push(profiled(&ts(3000, 0), "UADA00001", "Ada", "after the read"));
        list.push(profiled(&ts(3001, 0), "UADA00001", "Ada", "and again"));
    });
    let first = client
        .fetch_timeline(
            &channel(),
            &TimelineRequest {
                after: Some(cursor),
                ..page_request(TimelineView::Main, 1)
            },
        )
        .await
        .expect("delta");
    assert_eq!(ts_list(&first.messages), [ts(3000, 0)]);
    let said = first.delta.as_ref().expect("a delta");
    assert!(said.more && !said.complete, "{said:?}");
    assert!(!first.has_more && first.next_before.is_none());
    assert!(fake
        .requests("conversations.history")
        .last()
        .is_some_and(|params| params.contains_key("oldest")));
    let second = client
        .fetch_timeline(
            &channel(),
            &TimelineRequest {
                after: first.next_after.clone(),
                ..page_request(TimelineView::Main, 1)
            },
        )
        .await
        .expect("the rest");
    assert_eq!(ts_list(&second.messages), [ts(3001, 0)]);
    assert!(!second.delta.expect("a delta").more);

    let thread = TimelineRequest {
        thread_id: Some(r2.clone()),
        ..page_request(TimelineView::Thread, 25)
    };
    let newest = client
        .fetch_timeline(&channel(), &thread)
        .await
        .expect("thread");
    let cursor = newest
        .next_after
        .expect("a thread is read forward natively");
    fake.with(|state| {
        state
            .replies
            .get_mut(&(CH.to_owned(), r2.clone()))
            .expect("replies")
            .push(
                json!({"type": "message", "ts": ts(5000, 0), "thread_ts": r2, "user": "UREPLIER1",
                "text": "a reply after the read", "user_profile": {"display_name": "Replier"}}),
            );
    });
    let delta = client
        .fetch_timeline(
            &channel(),
            &TimelineRequest {
                after: Some(cursor.clone()),
                ..thread.clone()
            },
        )
        .await
        .expect("thread delta");
    assert_eq!(
        ts_list(&delta.messages),
        [ts(5000, 0)],
        "the root or an old reply came back"
    );
    assert_eq!(delta.thread.expect("the current summary").id, r2);
    // Bound to the view it was issued for.
    let crossed = client
        .fetch_timeline(
            &channel(),
            &TimelineRequest {
                after: Some(cursor),
                ..page_request(TimelineView::Main, 7)
            },
        )
        .await
        .expect_err("a thread's cursor read as Main's");
    assert!(
        matches!(crossed.cause(), ChatError::CursorExpired(_)),
        "{crossed}"
    );
    // A cursor this backend did not issue for this read — a bridge's, from before the channel was
    // configured here, or one carried over a change to what the view is — is gone, not a bad
    // request: the page reads the newest page instead of failing every refresh with it.
    for request in [
        TimelineRequest {
            after: Some("rev-1".to_owned()),
            ..page_request(TimelineView::Main, 7)
        },
        TimelineRequest {
            after: first.next_after.clone(),
            ..page_request(TimelineView::Threads, 7)
        },
    ] {
        let error = client
            .fetch_timeline(&channel(), &request)
            .await
            .expect_err("a cursor this backend cannot continue");
        assert!(
            matches!(error.cause(), ChatError::CursorExpired(_)),
            "{:?}: {error}",
            request.view
        );
    }
    for view in [TimelineView::Threads, TimelineView::Flat] {
        let page = client
            .fetch_timeline(&channel(), &page_request(view, 7))
            .await
            .expect("page");
        assert_eq!(
            page.next_after, None,
            "{view:?} issued a native forward cursor"
        );
    }
}
