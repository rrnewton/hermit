//! Test scaffolding for the cache: messages, a controllable clock, and an open cache in a private
//! scratch directory.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::{
    CacheOptions, ChanKey, Clock, Incoming, Limits, ListEnd, ListStart, Listing, MessageCache,
    Observation, Proof, RouteSnapshot, Scope,
};
use crate::model::{ChannelId, Message, MessageId, UserId};
use crate::threads::MessageThread;

/// A plain message with a creation time in milliseconds.
pub(crate) fn message(id: &str, author: &str, content: &str, created_ms: i64) -> Message {
    Message {
        thread: None,
        id: MessageId(id.to_owned()),
        channel_id: ChannelId("C1".to_owned()),
        author: author.to_owned(),
        author_id: UserId(format!("U-{author}")),
        author_is_bot: false,
        timestamp: crate::clock::iso_from_ms(created_ms),
        spoken_time: String::new(),
        reply_to: None,
        content: content.to_owned(),
        spoken_content: String::new(),
        content_html: String::new(),
        noise: false,
        reactions: None,
        heading: None,
    }
}

/// A reply in thread `thread` (whose root is `root`).
pub(crate) fn reply(id: &str, thread: &str, content: &str, created_ms: i64) -> Message {
    Message {
        thread: Some(MessageThread {
            id: thread.to_owned(),
            root_message_id: Some(MessageId(thread.to_owned())),
            is_root: false,
            reply_count: None,
            reply_count_exact: false,
        }),
        ..message(id, "replier", content, created_ms)
    }
}

/// A clock a test moves by hand.
#[derive(Debug)]
pub(crate) struct ManualClock(pub(crate) AtomicI64);

impl ManualClock {
    pub(crate) fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Provider-time origin for tests: 2026-10-01T00:00:00Z.
pub(crate) const T0: i64 = 1_790_812_800_000;

/// An open cache in a private directory, routed for channel `C1` of provider `p`.
pub(crate) struct Fixture {
    pub(crate) cache: MessageCache,
    pub(crate) clock: Arc<ManualClock>,
    pub(crate) chan: ChanKey,
    pub(crate) dir: crate::testing::TempDir,
}

impl Fixture {
    pub(crate) fn new(tag: &str) -> Self {
        Self::with_limits(tag, Limits::default())
    }

    pub(crate) fn with_limits(tag: &str, limits: Limits) -> Self {
        let dir = crate::testing::TempDir::new(tag);
        let clock = Arc::new(ManualClock(AtomicI64::new(T0)));
        let cache = open(&dir, &clock, limits);
        let chan = ChanKey::new("p", "C1");
        cache.retain_only(RouteSnapshot::new(
            [chan.clone(), ChanKey::new("p", "C2")],
            ["p".to_owned()],
        ));
        Self {
            cache,
            clock,
            chan,
            dir,
        }
    }

    pub(crate) fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("cache").join("c.sqlite3")
    }

    /// Offer, then wait for it to land.
    pub(crate) fn offer(&self, chan: &ChanKey, observation: Observation) {
        let stamp = self.cache.stamp(chan);
        self.cache.offer(chan.clone(), stamp, observation);
        self.settle();
    }

    /// Wait for everything offered to be applied, and for the background work it left (the rows
    /// of an erased channel, merges, checkpoints) to be done.
    pub(crate) fn settle(&self) {
        assert!(
            self.cache.quiesce_blocking(Duration::from_secs(30)),
            "the writer did not catch up: {:?}",
            self.cache.stats()
        );
    }

    /// A listing of `C1` main.
    pub(crate) fn list(&self, rows: Vec<Message>, observed_ms: i64, proof: Option<Proof>) {
        self.list_scope(Scope::Main, rows, observed_ms, proof);
    }

    pub(crate) fn list_scope(
        &self,
        scope: Scope,
        rows: Vec<Message>,
        observed_ms: i64,
        proof: Option<Proof>,
    ) {
        let home = scope.clone();
        self.offer(
            &self.chan,
            Observation::Listing(Listing {
                scope,
                rows: rows
                    .into_iter()
                    .map(|m| Incoming::new(m, home.clone()))
                    .collect(),
                observed_ms,
                proof,
            }),
        );
    }

    pub(crate) fn ids(&self, scope: &Scope) -> Vec<String> {
        self.cache
            .newest(&self.chan, scope, 10_000)
            .served()
            .expect("served")
            .into_iter()
            .map(|m| m.message.id.0)
            .collect()
    }
}

pub(crate) fn open(
    dir: &crate::testing::TempDir,
    clock: &Arc<ManualClock>,
    limits: Limits,
) -> MessageCache {
    MessageCache::open(CacheOptions {
        limits,
        clock: Arc::clone(clock) as Arc<dyn Clock>,
        ..CacheOptions::new(dir.path().join("cache").join("c.sqlite3"))
    })
    .expect("the cache opens")
}

/// A proof for a read of the newest page.
pub(crate) fn newest_proof(from: ListStart) -> Proof {
    Proof {
        from,
        until: ListEnd::Newest,
        complete: true,
        full: true,
        source: super::coverage::Source::Read,
    }
}
