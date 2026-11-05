//! What one read of a Slack conversation's Flat view costs in Web API calls, counted at a loopback
//! fake that answers 20 rows a call, as the bridge in front of Slack does. `#282 slack-flat-skip`,
//! remote-ops audit W8.
//!
//! Flat is the page's default view, and on Slack every poll of it is a full read. Each one used to
//! walk the replies of up to ten threads from their first reply: 3 + Σ min(ceil((R+1)/20), 10)
//! calls, 23 for ten threads of thirty replies, every 45 seconds. A root carries its `reply_count`
//! and `latest_reply`, and the read has just fetched every root on the page, so a thread whose root
//! says what it said last time has nothing new to read, and one that moved needs only what came
//! after the newest reply already held.

mod slack_fake;

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use slack_fake::{profiled, thread, ts, Fake, FakeState};
use vibe_talk::chat::ChatClient;
use vibe_talk::model::{ChannelId, Message};
use vibe_talk::slack::{ts_from_message_id, HttpSlackClient};
use vibe_talk::threads::{TimelineRequest, TimelineView};

const CH: &str = "C0000MAIN1";
/// Rows the fake answers per call, as the bridge in front of Slack does.
const PAGE_CAP: usize = 20;
/// Replies in each thread: two calls each to walk in full at [`PAGE_CAP`].
const REPLIES: i64 = 30;

fn channel() -> ChannelId {
    ChannelId(CH.to_owned())
}

fn ts_list(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| ts_from_message_id(&message.id).expect("a Slack-encoded id"))
        .collect()
}

/// The `ts` of reply `index` in the thread rooted on message `root` of [`ten_threads`].
fn reply_ts(root: i64, index: i64) -> String {
    ts(1000 + root * 100 + index, 0)
}

/// Fifty messages, every fifth the root of a thread of [`REPLIES`] replies: one Flat page of
/// fifty holds all ten roots. Returns the roots' `ts` and each root's message index.
fn ten_threads() -> (FakeState, Vec<(String, i64)>) {
    let mut state = FakeState::default();
    let mut list = Vec::new();
    let mut roots = Vec::new();
    for i in 0..50 {
        let at = ts(i * 10, 0);
        let mut message = profiled(&at, "UADA00001", "Ada", &format!("message {i}"));
        if i % 5 == 0 {
            let replies: Vec<(String, String)> = (0..REPLIES)
                .map(|r| (reply_ts(i, r), format!("reply {r} to {i}")))
                .collect();
            thread(&mut state, CH, &mut message, &replies);
            roots.push((at, i));
        }
        list.push(message);
    }
    state.history.insert(CH.to_owned(), list);
    (state, roots)
}

/// Make the root `root` say what Slack's would after its replies changed.
fn restate_root(state: &mut FakeState, root: &str) {
    let replies = &state.replies[&(CH.to_owned(), root.to_owned())];
    let count = replies.len();
    let latest = replies.last().map(|reply| reply["ts"].clone());
    let message = state
        .history
        .get_mut(CH)
        .expect("history")
        .iter_mut()
        .find(|message| message["ts"] == json!(root))
        .expect("root");
    message["reply_count"] = json!(count);
    message["latest_reply"] = latest.unwrap_or(serde_json::Value::Null);
}

/// Post a reply in `root`'s thread, as somebody else would.
fn add_reply(fake: &Fake, root: &str, at: &str) {
    fake.with(|state| {
        state
            .replies
            .get_mut(&(CH.to_owned(), root.to_owned()))
            .expect("thread")
            .push(
                json!({"type": "message", "ts": at, "thread_ts": root, "user": "UREPLIER1",
                "text": format!("news at {at}"), "user_profile": {"display_name": "Replier"}}),
            );
        restate_root(state, root);
    });
}

/// What one Flat read returned, and the Web API calls it made.
struct Read {
    messages: Vec<String>,
    /// Each message's text, in the same order.
    texts: Vec<String>,
    history: usize,
    /// Every `conversations.replies` request the read sent, in order.
    replies: Vec<HashMap<String, String>>,
}

impl Read {
    fn calls(&self) -> usize {
        self.history + self.replies.len()
    }

    /// The threads whose replies the read asked for, in order.
    fn threads(&self) -> Vec<String> {
        self.replies
            .iter()
            .map(|params| params["ts"].clone())
            .collect()
    }

    /// Whether the read returned a message that says exactly `text`.
    fn shows(&self, text: &str) -> bool {
        self.texts.iter().any(|shown| shown == text)
    }
}

async fn flat(fake: &Fake, client: &HttpSlackClient) -> Read {
    let history = fake.calls("conversations.history");
    let replies = fake.requests("conversations.replies").len();
    let page = client
        .fetch_timeline(
            &channel(),
            &TimelineRequest {
                view: TimelineView::Flat,
                thread_id: None,
                before: None,
                after: None,
                limit: 50,
            },
        )
        .await
        .expect("flat");
    let messages = ts_list(&page.messages);
    assert!(
        messages.windows(2).all(|pair| pair[0] < pair[1]),
        "oldest first, each message once"
    );
    Read {
        messages,
        texts: page
            .messages
            .iter()
            .map(|message| message.content.clone())
            .collect(),
        history: fake.calls("conversations.history") - history,
        replies: fake.requests("conversations.replies")[replies..].to_vec(),
    }
}

#[tokio::test]
async fn a_flat_read_again_reads_replies_only_for_the_threads_that_moved() {
    for root_always in [false, true] {
        only_the_threads_that_moved(root_always).await;
    }
}

/// `root_always`: the server repeats the root on every replies page, as some do.
async fn only_the_threads_that_moved(root_always: bool) {
    let (mut state, roots) = ten_threads();
    state.root_always = root_always;
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();

    let first = flat(&fake, &client).await;
    assert_eq!(first.messages.len(), 50 + 10 * 30, "every root's replies");
    assert_eq!(
        (first.history, first.replies.len()),
        (3, 20),
        "the first read walks every thread: 3 history pages and 2 replies pages per thread"
    );
    assert_eq!(first.calls(), 23, "the audit's count for ten threads of 30");

    // Nothing new: no thread is read.
    let again = flat(&fake, &client).await;
    assert_eq!(again.messages, first.messages);
    assert_eq!(
        again.history, 3,
        "history is read every time, roots and all"
    );
    assert!(
        again.replies.is_empty(),
        "no thread moved, yet these were read: {:?}",
        again.threads()
    );

    // One new reply: one call, for that thread, from the newest reply already held.
    let (moved, index) = roots[3].clone();
    let news = ts(9000, 0);
    add_reply(&fake, &moved, &news);
    let one = flat(&fake, &client).await;
    assert_eq!(
        one.threads(),
        std::slice::from_ref(&moved),
        "only the thread that moved"
    );
    assert_eq!(
        one.replies[0].get("oldest"),
        Some(&reply_ts(index, REPLIES - 1)),
        "read from the newest reply already held"
    );
    let mut expected = first.messages.clone();
    expected.push(news.clone());
    expected.sort();
    assert_eq!(one.messages, expected, "the new reply, and nothing lost");
    assert_eq!(one.calls(), 3 + 1);

    // What that read brought is held: the next read reads nothing again.
    let settled = flat(&fake, &client).await;
    assert!(settled.replies.is_empty(), "{:?}", settled.threads());
    assert_eq!(settled.messages, expected);

    // A deletion leaves the count short of what was held: that thread is read in full.
    let (pruned, index) = roots[7].clone();
    let gone = reply_ts(index, 4);
    fake.with(|state| {
        state
            .replies
            .get_mut(&(CH.to_owned(), pruned.clone()))
            .expect("thread")
            .retain(|reply| reply["ts"] != json!(gone));
        restate_root(state, &pruned);
    });
    let deleted = flat(&fake, &client).await;
    assert_eq!(deleted.threads(), [pruned.clone(), pruned.clone()]);
    assert!(
        deleted
            .replies
            .iter()
            .all(|params| !params.contains_key("oldest")),
        "a deletion is read in full, not forward"
    );
    expected.retain(|at| *at != gone);
    assert_eq!(deleted.messages, expected, "the deleted reply is gone");

    // One reply added and another deleted leave the count where it was: read in full as well.
    let (churned, index) = roots[1].clone();
    let swapped = reply_ts(index, 10);
    let late = ts(9100, 0);
    fake.with(|state| {
        state
            .replies
            .get_mut(&(CH.to_owned(), churned.clone()))
            .expect("thread")
            .retain(|reply| reply["ts"] != json!(swapped));
    });
    add_reply(&fake, &churned, &late);
    let churn = flat(&fake, &client).await;
    assert_eq!(churn.threads(), [churned.clone(), churned.clone()]);
    expected.retain(|at| *at != swapped);
    expected.push(late);
    expected.sort();
    assert_eq!(churn.messages, expected);

    // More new replies than Flat shows of a thread: the newest of them, read in full at once,
    // never the oldest of the new ones.
    let (flooded, _) = roots[9].clone();
    for second in 0..60 {
        add_reply(&fake, &flooded, &ts(10_000 + second, 0));
    }
    let flood = flat(&fake, &client).await;
    assert_eq!(flood.replies.len(), 5, "the root and 90 replies, 20 a call");
    assert!(
        flood
            .replies
            .iter()
            .all(|params| params["ts"] == flooded && !params.contains_key("oldest")),
        "{:?}",
        flood.replies
    );
    for second in 10..60 {
        assert!(flood.messages.contains(&ts(10_000 + second, 0)));
    }
    assert!(
        !flood.messages.contains(&ts(10_000 + 9, 0)),
        "the newest 50 only"
    );
}

/// Remembered replies are trusted for the page's reconcile period, fifteen minutes by default, and
/// then read in full again: that is how an edit to a reply, which changes nothing on its root,
/// reaches Flat. Here the period is cut to a tenth of a second.
#[tokio::test]
async fn remembered_replies_are_read_in_full_again_after_the_reconcile_period() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake
        .client()
        .with_flat_reconcile(Duration::from_millis(100));
    let first = flat(&fake, &client).await;
    assert_eq!(first.replies.len(), 20);

    let (edited, index) = roots[0].clone();
    let target = reply_ts(index, 29);
    fake.with(|state| {
        let reply = state
            .replies
            .get_mut(&(CH.to_owned(), edited.clone()))
            .expect("thread")
            .iter_mut()
            .find(|reply| reply["ts"] == json!(target))
            .expect("reply");
        reply["text"] = json!("edited after the read");
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let page = client
        .fetch_timeline(
            &channel(),
            &TimelineRequest {
                view: TimelineView::Flat,
                thread_id: None,
                before: None,
                after: None,
                limit: 50,
            },
        )
        .await
        .expect("flat");
    assert_eq!(
        fake.requests("conversations.replies").len(),
        40,
        "every thread read in full again"
    );
    assert!(page
        .messages
        .iter()
        .any(|message| message.content == "edited after the read"));

    // Reconciling every read is what Flat did before it remembered anything.
    let every = fake.client().with_flat_reconcile(Duration::ZERO);
    for _ in 0..2 {
        assert_eq!(flat(&fake, &every).await.replies.len(), 20);
    }
}

/// The `conversations.replies` requests that asked for `root`'s thread.
fn reads_of(read: &Read, root: &str) -> Vec<HashMap<String, String>> {
    read.replies
        .iter()
        .filter(|params| params["ts"] == root)
        .cloned()
        .collect()
}

/// A full read of a thread is held only when it agrees with the root the same Flat read fetched:
/// a replies feed that lags the history row (a replica or a cache in the bridge) leaves the
/// newest reply out, and the next read, the root unchanged, must not reuse that. Remote-ops audit
/// W8 review.
#[tokio::test]
async fn a_full_read_that_lagged_its_root_is_read_again() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    let (lagging, index) = roots[2].clone();
    let newest = reply_ts(index, REPLIES - 1);
    // The root says thirty replies, the newest of them latest; the feed has only twenty-nine.
    let hidden = fake.with(|state| {
        state
            .replies
            .get_mut(&(CH.to_owned(), lagging.clone()))
            .expect("thread")
            .pop()
            .expect("a reply")
    });
    let first = flat(&fake, &client).await;
    assert_eq!(first.calls(), 23);
    assert!(!first.messages.contains(&newest), "the feed lags");

    // The feed catches up. The root already said thirty, so it reads exactly as before.
    fake.with(|state| {
        state
            .replies
            .get_mut(&(CH.to_owned(), lagging.clone()))
            .expect("thread")
            .push(hidden);
    });
    let second = flat(&fake, &client).await;
    assert_eq!(
        second.threads(),
        [lagging.clone(), lagging.clone()],
        "only the thread whose read did not agree with its root, read in full"
    );
    assert!(reads_of(&second, &lagging)
        .iter()
        .all(|params| !params.contains_key("oldest")));
    assert!(
        second.messages.contains(&newest),
        "the reply the root reported is shown"
    );

    // That read agreed with its root: held from now on.
    let third = flat(&fake, &client).await;
    assert!(third.replies.is_empty(), "{:?}", third.threads());
    assert_eq!(third.messages, second.messages);
}

/// A reply posted after a Flat read fetched history and before it fetched that thread is newer
/// than the latest reply the root named, so that read is not held. Deleted again before the next
/// read, the root reads exactly as before, and the reply must not linger. Remote-ops audit W8
/// review.
#[tokio::test]
async fn a_reply_posted_during_a_read_and_deleted_is_not_kept() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    let (raced, _) = roots[4].clone();
    let racer = ts(9500, 0);
    // In the feed, but not yet on the root the read sees.
    fake.with(|state| {
        state
            .replies
            .get_mut(&(CH.to_owned(), raced.clone()))
            .expect("thread")
            .push(
                json!({"type": "message", "ts": racer, "thread_ts": raced, "user": "UREPLIER1",
                "text": "the wrong thread", "user_profile": {"display_name": "Replier"}}),
            );
    });
    let first = flat(&fake, &client).await;
    assert_eq!(first.calls(), 23);
    assert!(first.messages.contains(&racer));

    fake.with(|state| {
        state
            .replies
            .get_mut(&(CH.to_owned(), raced.clone()))
            .expect("thread")
            .retain(|reply| reply["ts"] != json!(racer));
    });
    let second = flat(&fake, &client).await;
    assert_eq!(second.threads(), [raced.clone(), raced.clone()]);
    assert!(
        !second.messages.contains(&racer),
        "a deleted reply is still shown"
    );

    let third = flat(&fake, &client).await;
    assert!(third.replies.is_empty(), "{:?}", third.threads());
    assert_eq!(third.messages, second.messages);
}

/// A root that carries `reply_count` but no `latest_reply` leaves the count as the only sign of
/// change, and one reply deleted and another added leave it where it was. Such a thread is read in
/// full on every Flat read, as before W8: no saving, and nothing stale. Remote-ops audit W8 review.
#[tokio::test]
async fn a_root_without_latest_reply_is_read_in_full_every_time() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    fake.with(|state| {
        for message in state.history.get_mut(CH).expect("history") {
            message
                .as_object_mut()
                .expect("an object")
                .remove("latest_reply");
        }
    });
    let client = fake.client();
    let first = flat(&fake, &client).await;
    assert_eq!(first.calls(), 23);

    let (churned, index) = roots[1].clone();
    let gone = reply_ts(index, 3);
    let added = ts(9200, 0);
    fake.with(|state| {
        let replies = state
            .replies
            .get_mut(&(CH.to_owned(), churned.clone()))
            .expect("thread");
        replies.retain(|reply| reply["ts"] != json!(gone));
        replies.push(
            json!({"type": "message", "ts": added, "thread_ts": churned, "user": "UREPLIER1",
            "text": "a new reply", "user_profile": {"display_name": "Replier"}}),
        );
    });
    let second = flat(&fake, &client).await;
    assert_eq!(
        (second.history, second.replies.len()),
        (3, 20),
        "every thread read in full, as before"
    );
    assert!(second.messages.contains(&added), "the new reply is shown");
    assert!(
        !second.messages.contains(&gone),
        "the deleted reply is gone"
    );
}

/// Open `root`'s thread, its newest `limit` messages, as the page's thread view does. Returns the
/// texts it shows and the `conversations.replies` calls it made.
async fn open_thread(
    fake: &Fake,
    client: &HttpSlackClient,
    root: &str,
    limit: u16,
) -> (Vec<String>, usize) {
    let before = fake.requests("conversations.replies").len();
    let page = client
        .fetch_timeline(
            &channel(),
            &TimelineRequest {
                view: TimelineView::Thread,
                thread_id: Some(root.to_owned()),
                before: None,
                after: None,
                limit,
            },
        )
        .await
        .expect("thread");
    let texts = page
        .messages
        .iter()
        .map(|message| message.content.clone())
        .collect();
    (texts, fake.requests("conversations.replies").len() - before)
}

/// An edit to a reply changes nothing on its root, so Flat keeps the text it holds. Once the
/// thread view has read the thread and shown the edit, the next Flat read reads that thread in
/// full and shows it too; a thread view that shows what Flat holds changes nothing, at no call.
/// Remote-ops audit W8 review.
#[tokio::test]
async fn a_thread_view_that_shows_an_edit_brings_it_into_flat() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    let first = flat(&fake, &client).await;
    assert_eq!(first.calls(), 23);

    // Read as Flat holds it: Flat still reads no thread afterwards. A thread view of thirty
    // replies costs its root and two pages, compared with what Flat holds or not.
    let (opened, index) = roots[0].clone();
    let (_, thread_calls) = open_thread(&fake, &client, &opened, 20).await;
    assert_eq!(thread_calls, 1 + 2);
    let same = flat(&fake, &client).await;
    assert!(same.replies.is_empty(), "{:?}", same.threads());

    let target = reply_ts(index, REPLIES - 2);
    fake.with(|state| {
        let reply = state
            .replies
            .get_mut(&(CH.to_owned(), opened.clone()))
            .expect("thread")
            .iter_mut()
            .find(|reply| reply["ts"] == json!(target))
            .expect("reply");
        reply["text"] = json!("edited after the read");
        reply["edited"] = json!({"user": "UREPLIER1", "ts": ts(9800, 0)});
    });
    let held = flat(&fake, &client).await;
    assert!(held.replies.is_empty(), "the root did not move");
    assert!(!held
        .texts
        .iter()
        .any(|text| text == "edited after the read"));

    let (shown, thread_calls) = open_thread(&fake, &client, &opened, 20).await;
    assert_eq!(thread_calls, 1 + 2, "noticing the edit costs no call");
    assert!(shown.iter().any(|text| text == "edited after the read"));
    let next = flat(&fake, &client).await;
    assert_eq!(
        next.threads(),
        [opened.clone(), opened.clone()],
        "that thread, read in full"
    );
    assert!(next
        .texts
        .iter()
        .any(|text| text == "edited after the read"));
    let settled = flat(&fake, &client).await;
    assert!(settled.replies.is_empty(), "{:?}", settled.threads());

    // A thread view of the newest replies only does not see an edit further back, and changes
    // nothing for it.
    let (back, back_index) = roots[5].clone();
    let early = reply_ts(back_index, 2);
    fake.with(|state| {
        let reply = state
            .replies
            .get_mut(&(CH.to_owned(), back.clone()))
            .expect("thread")
            .iter_mut()
            .find(|reply| reply["ts"] == json!(early))
            .expect("reply");
        reply["text"] = json!("edited far back");
    });
    let (shown, thread_calls) = open_thread(&fake, &client, &back, 10).await;
    assert_eq!(thread_calls, 1 + 2);
    assert!(!shown.iter().any(|text| text == "edited far back"));
    assert!(flat(&fake, &client).await.replies.is_empty());
}

/// Set the `reply_count` on `root`'s history row to `count`, leaving its `latest_reply` alone, as
/// a root that has caught up with a deletion its replies feed has not.
fn set_reply_count(fake: &Fake, root: &str, count: i64) {
    fake.with(|state| {
        let message = state
            .history
            .get_mut(CH)
            .expect("history")
            .iter_mut()
            .find(|message| message["ts"] == json!(root))
            .expect("root");
        message["reply_count"] = json!(count);
    });
}

/// Delete the reply `at` from `root`'s replies feed, leaving the root as it was.
fn delete_from_feed(fake: &Fake, root: &str, at: &str) {
    fake.with(|state| {
        state
            .replies
            .get_mut(&(CH.to_owned(), root.to_owned()))
            .expect("thread")
            .retain(|reply| reply["ts"] != json!(at));
    });
}

/// One reply deleted and two posted raise the count by one, but the forward read brings two: that
/// does not add up, so the thread is read in full and the deleted reply goes. Merging the two
/// regardless would keep it until the reconcile. Remote-ops audit W8 review.
#[tokio::test]
async fn an_incremental_read_that_does_not_add_up_reads_the_thread_in_full() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    assert_eq!(flat(&fake, &client).await.calls(), 23);

    let (churned, index) = roots[6].clone();
    let gone = reply_ts(index, 7);
    delete_from_feed(&fake, &churned, &gone);
    add_reply(&fake, &churned, &ts(9700, 0));
    add_reply(&fake, &churned, &ts(9701, 0));
    let read = flat(&fake, &client).await;
    assert_eq!(
        read.threads(),
        [churned.clone(), churned.clone(), churned.clone()],
        "one forward read that did not add up, then the root and 31 replies, 20 a call"
    );
    assert_eq!(
        read.replies[0].get("oldest"),
        Some(&reply_ts(index, REPLIES - 1))
    );
    assert!(read.replies[1..]
        .iter()
        .all(|params| !params.contains_key("oldest")));
    assert!(!read.messages.contains(&gone), "the deleted reply is gone");
    assert!(read.messages.contains(&ts(9700, 0)));
    assert!(read.messages.contains(&ts(9701, 0)));

    let settled = flat(&fake, &client).await;
    assert!(settled.replies.is_empty(), "{:?}", settled.threads());
    assert_eq!(settled.messages, read.messages);
}

/// A thread that gets a reply more often than the reconcile period is read forward each time,
/// and those forward reads do not restart its reconcile clock: it is still read in full once its
/// last FULL read is a period old, so an edit to an older reply reaches Flat. Here the period is
/// two seconds, and the reads are 1.1 seconds apart. Remote-ops audit W8 review.
#[tokio::test]
async fn a_forward_read_does_not_restart_the_reconcile_clock() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client().with_flat_reconcile(Duration::from_secs(2));
    assert_eq!(flat(&fake, &client).await.replies.len(), 20);

    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (busy, index) = roots[0].clone();
    add_reply(&fake, &busy, &ts(9900, 0));
    let step = flat(&fake, &client).await;
    assert_eq!(
        step.threads(),
        std::slice::from_ref(&busy),
        "one forward read"
    );
    assert!(step.replies[0].contains_key("oldest"));

    let target = reply_ts(index, 3);
    fake.with(|state| {
        let reply = state
            .replies
            .get_mut(&(CH.to_owned(), busy.clone()))
            .expect("thread")
            .iter_mut()
            .find(|reply| reply["ts"] == json!(target))
            .expect("reply");
        reply["text"] = json!("edited before the reconcile");
    });
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let late = flat(&fake, &client).await;
    assert!(
        reads_of(&late, &busy)
            .iter()
            .all(|params| !params.contains_key("oldest")),
        "read in full: {:?}",
        reads_of(&late, &busy)
    );
    assert!(
        late.texts
            .iter()
            .any(|text| text == "edited before the reconcile"),
        "the forward read restarted the reconcile clock"
    );
}

/// A feed that lags the root's newest reply, read just as another reply is posted, holds as many
/// replies as the root counts, but ends past the latest reply the root names. That is not held:
/// once the feed catches up and the newer reply is deleted, the root reads exactly as before, and
/// the next Flat read must show the reply the root names and not the deleted one. Remote-ops
/// audit W8 review.
#[tokio::test]
async fn a_full_read_past_its_roots_latest_is_read_again_even_when_the_count_matches() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    let (lagging, index) = roots[2].clone();
    let newest = reply_ts(index, REPLIES - 1);
    let racer = ts(9600, 0);
    let hidden = fake.with(|state| {
        let replies = state
            .replies
            .get_mut(&(CH.to_owned(), lagging.clone()))
            .expect("thread");
        let hidden = replies.pop().expect("a reply");
        replies.push(
            json!({"type": "message", "ts": racer, "thread_ts": lagging, "user": "UREPLIER1",
            "text": "racer", "user_profile": {"display_name": "Replier"}}),
        );
        hidden
    });
    let first = flat(&fake, &client).await;
    assert_eq!(first.calls(), 23);
    assert!(!first.messages.contains(&newest), "the feed lags");
    assert!(first.messages.contains(&racer));

    fake.with(|state| {
        let replies = state
            .replies
            .get_mut(&(CH.to_owned(), lagging.clone()))
            .expect("thread");
        replies.retain(|reply| reply["ts"] != json!(racer));
        replies.push(hidden);
    });
    let second = flat(&fake, &client).await;
    assert_eq!(second.threads(), [lagging.clone(), lagging.clone()]);
    assert!(
        second.messages.contains(&newest),
        "the reply the root names is shown"
    );
    assert!(!second.messages.contains(&racer), "a deleted reply lingers");
}

/// A thread of fewer replies than Flat shows, whose root already counts one reply fewer than its
/// replies feed still lists, ends at the latest reply the root names; only the count shows the
/// read is behind. It is not held, so once the feed catches up the deleted reply goes. Remote-ops
/// audit W8 review.
#[tokio::test]
async fn a_full_read_with_a_reply_its_root_no_longer_counts_is_read_again() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    let (pruned, index) = roots[8].clone();
    let gone = reply_ts(index, 5);
    set_reply_count(&fake, &pruned, REPLIES - 1);
    let first = flat(&fake, &client).await;
    assert_eq!(first.calls(), 23);
    assert!(first.messages.contains(&gone), "the feed lags the deletion");

    delete_from_feed(&fake, &pruned, &gone);
    let second = flat(&fake, &client).await;
    assert_eq!(second.threads(), [pruned.clone(), pruned.clone()]);
    assert!(
        !second.messages.contains(&gone),
        "a reply deleted in Slack is still shown"
    );

    let third = flat(&fake, &client).await;
    assert!(third.replies.is_empty(), "{:?}", third.threads());
    assert_eq!(third.messages, second.messages);
}

/// One message, the root of a thread of `replies` replies, reply `r` at `ts(1000 + r)`. Returns
/// the root's `ts`.
fn one_thread(replies: i64) -> (FakeState, String) {
    let mut state = FakeState::default();
    let root = ts(0, 0);
    let mut message = profiled(&root, "UADA00001", "Ada", "root");
    let list: Vec<(String, String)> = (0..replies)
        .map(|r| (ts(1000 + r, 0), format!("reply {r}")))
        .collect();
    thread(&mut state, CH, &mut message, &list);
    state.history.insert(CH.to_owned(), vec![message]);
    (state, root)
}

/// A thread of more replies than Flat shows is held only when the WHOLE thread, read in the same
/// calls, holds as many replies as its root counts: comparing the newest fifty Flat keeps with
/// `min(count, 50)` says nothing at all. Here the root already counts 79 after a deletion its
/// replies feed still lists; once the feed catches up, the root reads exactly as before, and the
/// deleted reply must go. Remote-ops audit W8 review.
#[tokio::test]
async fn a_long_thread_whose_feed_lags_a_deletion_is_read_again() {
    let (state, root) = one_thread(80);
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    let gone = ts(1000 + 60, 0);
    set_reply_count(&fake, &root, 79);
    let first = flat(&fake, &client).await;
    assert_eq!(
        first.replies.len(),
        5,
        "the root and 80 replies, 20 a call, read whole"
    );
    assert!(first.messages.contains(&gone), "the feed lags the deletion");

    delete_from_feed(&fake, &root, &gone);
    let second = flat(&fake, &client).await;
    assert_eq!(
        second.replies.len(),
        4,
        "read whole again: the root and 79 replies"
    );
    assert!(
        !second.messages.contains(&gone),
        "a reply deleted in Slack is still shown"
    );
    assert!(second.messages.contains(&ts(1000 + 79, 0)));
    assert!(
        !second.messages.contains(&ts(1000 + 28, 0)),
        "the newest 50"
    );

    // That read counted what the root counts: held from now on.
    let third = flat(&fake, &client).await;
    assert!(third.replies.is_empty(), "{:?}", third.threads());
    assert_eq!(third.messages, second.messages);
}

/// A thread too long to read whole in one window (ten pages, here 200 rows) is answered by
/// narrowing toward its newest replies, which reads only part of it, so there is no count to
/// compare with its root's: replies ending at the root's latest say nothing about a deletion the
/// replies feed has not caught up with. Such a thread is never held, and every Flat read reads it
/// as Flat did before W8. Remote-ops audit W8 review.
#[tokio::test]
async fn a_thread_too_long_to_count_is_read_on_every_flat_read() {
    let (state, root) = one_thread(250);
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    let first = flat(&fake, &client).await;
    assert!(first.replies.len() > 10, "a window, then narrowing");
    assert!(
        first.messages.contains(&ts(1000 + 249, 0)),
        "the newest reply, which the root names"
    );
    let second = flat(&fake, &client).await;
    assert_eq!(
        second.replies.len(),
        first.replies.len(),
        "read again, exactly as before"
    );
    assert!(second.threads().iter().all(|thread| *thread == root));
    assert_eq!(second.messages, first.messages);
}

/// One older reply deleted while a system event and a real reply are posted, the root counting
/// both: the count grows by one, and one real reply came back, ending at the root's latest. Only
/// counting EVERY message the forward read brought, the system event included, shows that the two
/// do not add up. The thread is read in full, and the deleted reply goes; and since its root
/// counts a reply Flat does not keep, it is never held, but read in full on every Flat read, as
/// before W8. Remote-ops audit W8 review.
#[tokio::test]
async fn a_deletion_behind_a_system_event_and_a_reply_is_read_in_full() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    let first = flat(&fake, &client).await;
    assert_eq!(first.calls(), 23);

    let (churned, index) = roots[3].clone();
    let gone = reply_ts(index, 5);
    delete_from_feed(&fake, &churned, &gone);
    fake.with(|state| {
        let mut event = slack_fake::system(&ts(9300, 0), "thread_event");
        event["thread_ts"] = json!(churned);
        state
            .replies
            .get_mut(&(CH.to_owned(), churned.clone()))
            .expect("thread")
            .push(event);
    });
    add_reply(&fake, &churned, &ts(9301, 0));
    let read = flat(&fake, &client).await;
    assert!(!read.messages.contains(&gone), "the deleted reply is gone");
    assert!(read.messages.contains(&ts(9301, 0)));
    assert!(!read.messages.contains(&ts(9300, 0)), "a system event");
    assert!(
        reads_of(&read, &churned)
            .iter()
            .any(|params| !params.contains_key("oldest")),
        "read in full: {:?}",
        reads_of(&read, &churned)
    );

    let again = flat(&fake, &client).await;
    assert_eq!(
        again.threads(),
        [churned.clone(), churned.clone()],
        "a count that includes a system event is never trusted"
    );
    assert_eq!(again.messages, read.messages);

    // A system event and a real reply, nothing deleted: the count grew by as many as came back,
    // but one of them is not a reply Flat keeps, so nothing is merged; the thread is read in full
    // and shows the reply alone.
    let (quiet, _) = roots[4].clone();
    fake.with(|state| {
        let mut event = slack_fake::system(&ts(9400, 0), "thread_event");
        event["thread_ts"] = json!(quiet);
        state
            .replies
            .get_mut(&(CH.to_owned(), quiet.clone()))
            .expect("thread")
            .push(event);
    });
    add_reply(&fake, &quiet, &ts(9401, 0));
    let mixed = flat(&fake, &client).await;
    assert!(mixed.messages.contains(&ts(9401, 0)));
    assert!(
        !mixed.messages.contains(&ts(9400, 0)),
        "a system event merged into Flat"
    );
    assert!(reads_of(&mixed, &quiet)
        .iter()
        .any(|params| !params.contains_key("oldest")));
}

/// Edit reply `index` of the thread rooted on `root` to say `text`, as Slack would: nothing on the
/// root changes.
fn edit_reply(fake: &Fake, root: &str, at: &str, text: &str) {
    fake.with(|state| {
        let reply = state
            .replies
            .get_mut(&(CH.to_owned(), root.to_owned()))
            .expect("thread")
            .iter_mut()
            .find(|reply| reply["ts"] == json!(at))
            .expect("reply");
        reply["text"] = json!(text);
        reply["edited"] = json!({"user": "UREPLIER1", "ts": ts(9800, 0)});
    });
}

/// Run one Flat read with the first replies request for `thread` answered late, its answer taken
/// from the thread as it was when the request arrived, and run `during` while it is held. Returns
/// that Flat read, whose requests include any `during` sent, and what `during` returned.
async fn flat_held_during<T>(
    fake: &Fake,
    client: &HttpSlackClient,
    thread: &str,
    during: impl std::future::Future<Output = T>,
) -> (Read, T) {
    let hold = slack_fake::Hold::new(thread);
    fake.with(|state| state.hold_replies = Some(hold.clone()));
    let driver = async {
        hold.arrived.notified().await;
        let out = during.await;
        hold.release.notify_one();
        out
    };
    tokio::join!(flat(fake, client), driver)
}

/// Run one Flat read with the first replies request for `thread` answered late, and while it is
/// held, edit `at` in that thread and open the thread view, which shows the edit. Returns that
/// Flat read, whose requests include the thread view's, and the texts the thread view showed.
async fn flat_while_thread_view_shows_an_edit(
    fake: &Fake,
    client: &HttpSlackClient,
    thread: &str,
    at: &str,
    text: &str,
) -> (Read, Vec<String>) {
    flat_held_during(fake, client, thread, async {
        edit_reply(fake, thread, at, text);
        open_thread(fake, client, thread, 20).await.0
    })
    .await
}

/// A Flat read already under way when the thread view shows an edit read that thread before the
/// edit, and must not store what it read once the thread view has spoken: here a forward read of
/// a thread that moved, which would put back the replies held from before. The next Flat read
/// reads that thread in full and shows the edit, as the thread view did. Remote-ops audit W8
/// review.
#[tokio::test]
async fn a_forward_read_under_way_when_the_thread_view_shows_an_edit_is_not_held() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    assert_eq!(flat(&fake, &client).await.calls(), 23);

    let (moved, index) = roots[0].clone();
    add_reply(&fake, &moved, &ts(9900, 0));
    let target = reply_ts(index, REPLIES - 2);
    let (racing, shown) =
        flat_while_thread_view_shows_an_edit(&fake, &client, &moved, &target, "edited mid-read")
            .await;
    assert!(shown.iter().any(|text| text == "edited mid-read"));
    assert_eq!(
        racing
            .replies
            .iter()
            .filter(|params| params.contains_key("oldest"))
            .count(),
        1,
        "one forward read, beside the thread view's three"
    );
    assert_eq!(racing.replies.len(), 1 + 3);
    assert!(racing.messages.contains(&ts(9900, 0)));

    let next = flat(&fake, &client).await;
    assert_eq!(
        next.threads(),
        [moved.clone(), moved.clone()],
        "that thread, read in full"
    );
    assert!(
        next.texts.iter().any(|text| text == "edited mid-read"),
        "the thread view showed an edit Flat still does not"
    );
}

/// The same, for a thread Flat has never read: its first full read is under way when the thread
/// view shows an edit to a reply that read had already passed, so nothing it read is held.
/// Remote-ops audit W8 review.
#[tokio::test]
async fn a_full_read_under_way_when_the_thread_view_shows_an_edit_is_not_held() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    let (opened, index) = roots[0].clone();
    // On the first of the thread's two pages, which is the one held.
    let target = reply_ts(index, 12);
    let (racing, shown) =
        flat_while_thread_view_shows_an_edit(&fake, &client, &opened, &target, "edited mid-read")
            .await;
    assert!(shown.iter().any(|text| text == "edited mid-read"));
    assert_eq!(racing.calls(), 23 + 3, "and the thread view's three");
    assert!(
        !racing.texts.iter().any(|text| text == "edited mid-read"),
        "the premise: that read began before the edit"
    );

    let next = flat(&fake, &client).await;
    assert_eq!(
        next.threads(),
        [opened.clone(), opened.clone()],
        "that thread, read in full"
    );
    assert!(
        next.texts.iter().any(|text| text == "edited mid-read"),
        "the thread view showed an edit Flat still does not"
    );
}

/// The reconcile period is counted from the instant a full read BEGAN, so a reply edited while
/// that read was under way, after it had passed the reply, reaches Flat within one period of the
/// edit: here a read held up for longer than the period is due again at once. Remote-ops audit W8
/// review.
#[tokio::test]
async fn the_reconcile_period_counts_from_when_a_full_read_began() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client().with_flat_reconcile(Duration::from_secs(1));
    let (slow, index) = roots[0].clone();
    let target = reply_ts(index, 12);
    let hold = slack_fake::Hold::new(&slow);
    fake.with(|state| state.hold_replies = Some(hold.clone()));
    let driver = async {
        hold.arrived.notified().await;
        edit_reply(&fake, &slow, &target, "edited mid-read");
        tokio::time::sleep(Duration::from_millis(1200)).await;
        hold.release.notify_one();
    };
    let (racing, ()) = tokio::join!(flat(&fake, &client), driver);
    assert_eq!(racing.calls(), 23);
    assert!(
        !racing.texts.iter().any(|text| text == "edited mid-read"),
        "the premise: that read began before the edit"
    );

    let next = flat(&fake, &client).await;
    assert_eq!(
        reads_of(&next, &slow).len(),
        2,
        "that thread, read in full at once"
    );
    assert!(next.texts.iter().any(|text| text == "edited mid-read"));
}

/// A reply the replies feed lists but its root does not yet count is the one thing trusting the
/// root can hide: the bound on it is that it shows once the root counts it, at one forward read,
/// and, if the root never does, at the thread's next full read, one reconcile period on. Here the
/// period is a second. Remote-ops audit W8 review.
#[tokio::test]
async fn a_reply_its_root_does_not_yet_count_shows_once_it_does_or_at_the_reconcile() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client().with_flat_reconcile(Duration::from_secs(1));
    assert_eq!(flat(&fake, &client).await.calls(), 23);

    // The root catches up: one forward read brings the reply.
    let (caught, _) = roots[3].clone();
    let news = ts(9000, 0);
    fake.with(|state| {
        state
            .replies
            .get_mut(&(CH.to_owned(), caught.clone()))
            .expect("thread")
            .push(
                json!({"type": "message", "ts": news, "thread_ts": caught, "user": "UREPLIER1",
                "text": "news", "user_profile": {"display_name": "Replier"}}),
            );
    });
    fake.with(|state| restate_root(state, &caught));
    let read = flat(&fake, &client).await;
    assert_eq!(read.threads(), std::slice::from_ref(&caught));
    assert!(read.messages.contains(&news));

    // The root never catches up: the reconcile brings it.
    let (lagging, _) = roots[5].clone();
    let late = ts(9100, 0);
    fake.with(|state| {
        state
            .replies
            .get_mut(&(CH.to_owned(), lagging.clone()))
            .expect("thread")
            .push(
                json!({"type": "message", "ts": late, "thread_ts": lagging, "user": "UREPLIER1",
                "text": "late", "user_profile": {"display_name": "Replier"}}),
            );
    });
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let reconciled = flat(&fake, &client).await;
    assert!(reconciled.messages.contains(&late));
}

/// What an edit between two overlapping Flat reads makes a reply say.
const EDITED: &str = "edited between the two reads";

/// Two Flat reads overlap, as they do for two devices or tabs polling one channel, or the page's
/// poll beside its read for counts: A reads a thread before an edit, B begins later, reads it
/// after the edit and shows it, and A finishes last. What A read is no newer than what B showed,
/// so A must not put its copy back over B's: the next Flat read shows the edit, as B did. Nothing
/// the client sees shows which of the two read the thread first, since A's answer arrives after
/// B's, so the next read may read that thread again; it reads no other. Remote-ops audit W8
/// review round 2.
#[tokio::test]
async fn an_older_flat_read_does_not_take_back_an_edit_a_newer_one_showed() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    let (thread, index) = roots[0].clone();
    // On the first of the thread's two pages, which is the one held.
    let target = reply_ts(index, 12);
    let (a, b) = flat_held_during(&fake, &client, &thread, async {
        edit_reply(&fake, &thread, &target, EDITED);
        flat(&fake, &client).await
    })
    .await;
    assert!(!a.shows(EDITED), "the premise: A read it before the edit");
    assert!(
        b.shows(EDITED),
        "the premise: B read it after, and showed it"
    );

    let next = flat(&fake, &client).await;
    assert!(
        next.shows(EDITED),
        "B showed an edit, and the next Flat read took it back"
    );
    assert!(
        next.threads().iter().all(|read| *read == thread),
        "only that thread, if any, is read again; the nine A and B read alike stay held"
    );
}

/// The same when B's read does not agree with B's root, here a root that briefly counts one reply
/// more than the thread holds, so B holds nothing for that thread: A, finishing last, must not
/// fill the place B left empty with its older copy. Remote-ops audit W8 review round 2.
#[tokio::test]
async fn an_older_flat_read_does_not_fill_the_place_a_newer_one_emptied() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    let (thread, index) = roots[0].clone();
    let target = reply_ts(index, 12);
    let (a, b) = flat_held_during(&fake, &client, &thread, async {
        edit_reply(&fake, &thread, &target, EDITED);
        set_reply_count(&fake, &thread, REPLIES + 1);
        let b = flat(&fake, &client).await;
        set_reply_count(&fake, &thread, REPLIES);
        b
    })
    .await;
    assert!(!a.shows(EDITED), "the premise: A read it before the edit");
    assert!(
        b.shows(EDITED),
        "the premise: B read it after, and showed it"
    );

    let next = flat(&fake, &client).await;
    assert!(
        next.shows(EDITED),
        "B showed an edit, and the next Flat read took it back"
    );
    assert_eq!(next.threads(), [thread.clone(), thread.clone()]);
}

/// The same for two forward reads of a thread that moved: A reads the new reply, it is edited,
/// and B reads it edited and shows it. Both build on the same full read, so when each thread was
/// last read in full cannot order them, and A must still not put back its copy of the new reply.
/// Remote-ops audit W8 review round 2.
#[tokio::test]
async fn an_older_forward_read_does_not_take_back_an_edit_a_newer_one_showed() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    assert_eq!(flat(&fake, &client).await.calls(), 23);

    let (moved, _) = roots[0].clone();
    let news = ts(9900, 0);
    add_reply(&fake, &moved, &news);
    let (a, b) = flat_held_during(&fake, &client, &moved, async {
        edit_reply(&fake, &moved, &news, EDITED);
        flat(&fake, &client).await
    })
    .await;
    assert_eq!(
        a.replies
            .iter()
            .filter(|params| params.contains_key("oldest"))
            .count(),
        2,
        "the premise: two forward reads, A's and B's"
    );
    assert!(!a.shows(EDITED), "the premise: A read it before the edit");
    assert!(
        b.shows(EDITED),
        "the premise: B read it after, and showed it"
    );

    let next = flat(&fake, &client).await;
    assert!(
        next.shows(EDITED),
        "B showed an edit, and the next Flat read took it back"
    );
    assert!(
        next.threads().iter().all(|read| *read == moved),
        "only that thread, if any, is read again"
    );
}

/// Which of two overlapping Flat reads began first says nothing about which read a thread first.
/// Here A begins first, but a rate limit holds it back; B begins later and reads the thread before
/// an edit; A then reads it after the edit and shows it. B finishes last, and must not put its
/// copy back over A's. Remote-ops audit W8 review round 2.
#[tokio::test]
async fn a_later_flat_read_that_read_the_thread_sooner_does_not_take_back_an_edit() {
    let (state, root) = one_thread(REPLIES);
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    // On the first of the thread's two pages.
    let target = ts(1000 + 12, 0);
    // A's first request for the thread is refused with a rate limit, answered late; A reads the
    // thread when it tries again, 50 ms after that answer.
    let limited = slack_fake::Hold::new(&root);
    fake.with(|state| {
        state.hold_replies = Some(limited.clone());
        state.rate_limit_once = Some(("conversations.replies".to_owned(), 0));
    });
    let a_done = tokio::sync::Notify::new();
    let a = async {
        let read = flat(&fake, &client).await;
        a_done.notify_one();
        read
    };
    let driver = async {
        limited.arrived.notified().await;
        let held = slack_fake::Hold::new(&root);
        fake.with(|state| state.hold_replies = Some(held.clone()));
        let steer = async {
            held.arrived.notified().await;
            edit_reply(&fake, &root, &target, EDITED);
            limited.release.notify_one();
            a_done.notified().await;
            held.release.notify_one();
        };
        let (b, ()) = tokio::join!(flat(&fake, &client), steer);
        b
    };
    let (a, b) = tokio::join!(a, driver);
    assert!(a.shows(EDITED), "the premise: A read it after the edit");
    assert!(
        !b.shows(EDITED),
        "the premise: B read it before, and finished last"
    );

    let next = flat(&fake, &client).await;
    assert!(
        next.shows(EDITED),
        "A showed an edit, and the next Flat read took it back"
    );
}

/// Two Flat reads that overlap but read a thread alike leave nothing to take back, so what the
/// first to finish held stays held, and the next read of that thread costs no call. Remote-ops
/// audit W8 review round 2.
#[tokio::test]
async fn overlapping_flat_reads_that_read_a_thread_alike_keep_it_held() {
    let (state, roots) = ten_threads();
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    assert_eq!(flat(&fake, &client).await.calls(), 23);

    let (moved, _) = roots[0].clone();
    let news = ts(9900, 0);
    add_reply(&fake, &moved, &news);
    let (a, b) = flat_held_during(&fake, &client, &moved, flat(&fake, &client)).await;
    assert!(a.messages.contains(&news) && b.messages.contains(&news));

    let next = flat(&fake, &client).await;
    assert!(next.replies.is_empty(), "nothing moved since: no call");
    assert!(next.messages.contains(&news));
}

/// The other way round: A reads a thread before an edit and finishes first, holding what it read;
/// B, begun before A finished, reads the thread after the edit, shows it, and finishes last.
/// Nothing the client sees says B read the thread after A did, so B holds nothing; but it must
/// drop A's copy, or the next Flat read takes back the edit B showed. Remote-ops audit W8 review
/// round 2.
#[tokio::test]
async fn a_flat_read_that_cannot_hold_its_copy_still_drops_an_older_one() {
    let (state, root) = one_thread(REPLIES);
    let fake = Fake::start(state, PAGE_CAP).await;
    let client = fake.client();
    // On the first of the thread's two pages.
    let target = ts(1000 + 12, 0);
    let first = slack_fake::Hold::new(&root);
    fake.with(|state| state.hold_replies = Some(first.clone()));
    let a_done = tokio::sync::Notify::new();
    let a = async {
        let read = flat(&fake, &client).await;
        a_done.notify_one();
        read
    };
    let driver = async {
        first.arrived.notified().await;
        edit_reply(&fake, &root, &target, EDITED);
        let second = slack_fake::Hold::new(&root);
        fake.with(|state| state.hold_replies = Some(second.clone()));
        let steer = async {
            second.arrived.notified().await;
            first.release.notify_one();
            a_done.notified().await;
            second.release.notify_one();
        };
        let (b, ()) = tokio::join!(flat(&fake, &client), steer);
        b
    };
    let (a, b) = tokio::join!(a, driver);
    assert!(
        !a.shows(EDITED),
        "the premise: A read it before the edit, and finished first"
    );
    assert!(
        b.shows(EDITED),
        "the premise: B read it after, and finished last"
    );

    let next = flat(&fake, &client).await;
    assert!(
        next.shows(EDITED),
        "B showed an edit, and the next Flat read took it back"
    );
    assert_eq!(next.threads(), [root.clone(), root.clone()]);
}
