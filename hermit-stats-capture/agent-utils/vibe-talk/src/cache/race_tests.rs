//! The erase protocol and the writer's ordering, at exact interleavings. `#208 persistent-cache`,
//! milestone `cache-store`.
//!
//! These tests hold the writer still with a pause, and run an erase at a chosen point of a read,
//! so what they check does not depend on timing.

use std::sync::{mpsc, Arc, Mutex as StdMutex};
use std::time::Duration;

use super::fixtures::{self, message, newest_proof, Fixture, T0};
use super::tests::{m, mid};
use super::*;
use crate::model::Message;

const SECONDS: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EraseKind {
    /// `erase_channels`.
    Channel,
    /// `erase_provider` of the channel's provider.
    Provider,
    /// A sweep to a snapshot that no longer lists it.
    Removal,
}

fn erase(cache: &MessageCache, chan: &ChanKey, kind: EraseKind) {
    match kind {
        EraseKind::Channel => cache.erase_channels(std::slice::from_ref(chan)),
        EraseKind::Provider => cache.erase_provider(&chan.provider_key),
        EraseKind::Removal => cache.retain_only(RouteSnapshot::new(
            [ChanKey::new("q", "D1")],
            ["q".to_owned()],
        )),
    }
}

fn rows_of(fx: &Fixture, chan: &ChanKey, n: i64) {
    fx.offer(
        chan,
        Observation::Rows {
            rows: (0..n).map(|i| Incoming::new(m(i), Scope::Main)).collect(),
            scope: None,
            observed_ms: T0,
        },
    );
}

/// The race the read's ordering exists for: an erase may land at any moment of a read, and the
/// writer may apply it before the read looks again. Whatever the moment, and whichever kind of
/// erase, the read never serves the erased channel's rows.
#[test]
fn a_read_never_serves_an_erased_channel_wherever_the_erase_lands() {
    for kind in [EraseKind::Channel, EraseKind::Provider, EraseKind::Removal] {
        for point in [
            ReadPoint::Start,
            ReadPoint::EpochTaken,
            ReadPoint::Checked,
            ReadPoint::SqlDone,
        ] {
            let fx = Fixture::new("cache-race-read");
            fx.list((0..30).map(m).collect(), T0, None);
            let resume = Arc::new(StdMutex::new(Some(fx.cache.pause_writer())));
            let release = Arc::clone(&resume);
            let chan = fx.chan.clone();
            let mut erased = false;
            fx.cache.set_read_hook(Some(Box::new(move |at, cache| {
                if at == point && !erased {
                    erased = true;
                    erase(cache, &chan, kind);
                }
                if at == ReadPoint::SqlDone {
                    // The worst case for a read with rows in hand: the writer applies the erase
                    // before the read checks anything again.
                    drop(release.lock().expect("lock").take());
                    assert!(cache.barrier_blocking(SECONDS));
                }
            })));
            let answer = fx.cache.newest(&fx.chan, &Scope::Main, 100);
            fx.cache.set_read_hook(None);
            drop(resume.lock().expect("lock").take());
            match &answer {
                Read::Served(rows) => assert!(
                    rows.is_empty(),
                    "{kind:?} at {point:?}: {} erased rows served",
                    rows.len()
                ),
                Read::Blocked | Read::Moved => {}
                other => panic!("{kind:?} at {point:?}: {other:?}"),
            }
            if point == ReadPoint::Start {
                assert_eq!(
                    answer,
                    Read::Blocked,
                    "{kind:?}: an erase asked for before the read began blocks it"
                );
            }
            fx.settle();
            assert_eq!(
                fx.cache.counts().served().expect("counts").messages,
                0,
                "{kind:?} at {point:?}"
            );
        }
    }
}

/// A channel taken off the list and put back before the writer has applied the sweep still has
/// the rows that sweep will erase: they are not served in between. A channel listed throughout
/// is unaffected.
#[test]
fn a_channel_taken_off_and_put_back_before_its_sweep_lands_is_not_served_meanwhile() {
    let fx = Fixture::new("cache-race-sweep");
    let other = ChanKey::new("p", "C2");
    fx.list((0..10).map(m).collect(), T0, None);
    rows_of(&fx, &other, 3);
    let resume = fx.cache.pause_writer();
    fx.cache
        .retain_only(RouteSnapshot::new([other.clone()], ["p".to_owned()]));
    fx.cache.retain_only(RouteSnapshot::new(
        [fx.chan.clone(), other.clone()],
        ["p".to_owned()],
    ));
    assert_eq!(fx.cache.newest(&fx.chan, &Scope::Main, 100), Read::Blocked);
    assert_eq!(
        fx.cache
            .newest(&other, &Scope::Main, 100)
            .served()
            .expect("listed throughout")
            .len(),
        3
    );
    drop(resume);
    fx.settle();
    assert!(
        fx.ids(&Scope::Main).is_empty(),
        "the sweep erased what was held"
    );
}

/// An erase the writer could not carry out leaves its channel blocked, not served: here a
/// trigger refuses every change to a channel row, and then the writer is gone altogether.
#[test]
fn an_erase_that_was_never_applied_keeps_its_channel_blocked() {
    let fx = Fixture::new("cache-race-failed");
    fx.list((0..10).map(m).collect(), T0, None);
    let sabotage = rusqlite::Connection::open(fx.path()).expect("open");
    sabotage
        .execute_batch(
            "CREATE TRIGGER refuse_update BEFORE UPDATE ON channel
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;
             CREATE TRIGGER refuse_delete BEFORE DELETE ON channel
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .expect("sabotage");
    drop(sabotage);
    fx.cache.erase_channels(std::slice::from_ref(&fx.chan));
    fx.settle();
    assert_eq!(fx.cache.newest(&fx.chan, &Scope::Main, 100), Read::Blocked);
    let held = super::tests::sql_count(&fx.cache, "SELECT COUNT(*) FROM message");
    assert_eq!(held, 10, "the erase did fail");

    let fx = Fixture::new("cache-race-dead");
    fx.list((0..10).map(m).collect(), T0, None);
    fx.cache.stop_writer();
    fx.cache.erase_channels(std::slice::from_ref(&fx.chan));
    assert_eq!(fx.cache.newest(&fx.chan, &Scope::Main, 100), Read::Blocked);
    fx.cache.erase_provider("p");
    let other = ChanKey::new("p", "C2");
    assert_eq!(fx.cache.newest(&other, &Scope::Main, 100), Read::Blocked);
}

/// A provider erase blocks reads of the provider's channels until the writer has applied it,
/// and makes every stamp taken before it stale.
#[test]
fn a_provider_erase_blocks_its_channels_meanwhile_and_stales_their_stamps() {
    let fx = Fixture::new("cache-race-provider");
    let other = ChanKey::new("p", "C2");
    fx.list((0..10).map(m).collect(), T0, None);
    let before = fx.cache.stamp(&other);
    let resume = fx.cache.pause_writer();
    fx.cache.erase_provider("p");
    assert_eq!(fx.cache.newest(&fx.chan, &Scope::Main, 100), Read::Blocked);
    assert_eq!(fx.cache.newest(&other, &Scope::Main, 100), Read::Blocked);
    drop(resume);
    fx.cache.offer(
        other.clone(),
        before,
        Observation::Rows {
            rows: vec![Incoming::new(m(1), Scope::Main)],
            scope: None,
            observed_ms: T0,
        },
    );
    fx.settle();
    assert_eq!(
        fx.cache.stats().ops_stale,
        1,
        "a stamp from before the erase"
    );
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 0);
}

/// The writer's sweep makes stale a stamp from before it even for a channel this process never
/// stamped or routed (it is in the file from an earlier run): a fill that started before the
/// sweep must not land once the channel is listed again.
#[test]
fn a_sweep_stales_stamps_for_channels_this_process_never_routed() {
    let fx = Fixture::new("cache-race-unknown");
    let other = ChanKey::new("p", "C2");
    rows_of(&fx, &other, 2);
    let Fixture {
        cache, clock, dir, ..
    } = fx;
    drop(cache);
    let cache = fixtures::open(&dir, &clock, Limits::default());
    let before = cache.stamp(&other);
    cache.retain_only(RouteSnapshot::new(
        [ChanKey::new("p", "C1")],
        ["p".to_owned()],
    ));
    assert!(cache.quiesce_blocking(SECONDS));
    cache.retain_only(RouteSnapshot::new(
        [ChanKey::new("p", "C1"), other.clone()],
        ["p".to_owned()],
    ));
    cache.offer(
        other.clone(),
        before,
        Observation::Rows {
            rows: vec![Incoming::new(m(5), Scope::Main)],
            scope: None,
            observed_ms: T0,
        },
    );
    assert!(cache.quiesce_blocking(SECONDS));
    assert_eq!(cache.stats().ops_stale, 1);
    assert_eq!(cache.counts().served().expect("counts").messages, 0);
}

/// Every byte an offer takes from the queue's budget is given back when it is applied, so a
/// cache that has taken in more than the budget in total still takes offers.
#[test]
fn the_writer_gives_back_queue_bytes_so_offers_past_the_budget_still_land() {
    let fx = Fixture::new("cache-race-budget");
    let body = "x".repeat(QUEUE_BYTES / 32);
    for i in 0..20 {
        let big = Message {
            content: format!("{body}{i}"),
            ..m(i)
        };
        fx.offer(
            &fx.chan,
            Observation::Rows {
                rows: vec![Incoming::new(big, Scope::Main)],
                scope: None,
                observed_ms: T0,
            },
        );
        assert_eq!(
            fx.cache.queued_bytes(),
            0,
            "offer {i}'s bytes were not given back"
        );
    }
    assert_eq!(fx.cache.stats().ops_dropped, 0);
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 20);
}

/// After a dropped offer, only a read of the newest end that STARTED after the drop makes the
/// channel's head trusted again.
#[test]
fn only_a_read_of_the_newest_end_started_after_the_drop_clears_a_suspect_channel() {
    let fx = Fixture::new("cache-race-suspect");
    fx.clock.set(T0 + 1_000);
    let huge = Message {
        content: "x".repeat(QUEUE_BYTES),
        ..m(1)
    };
    let stamp = fx.cache.stamp(&fx.chan);
    let offered = fx.cache.offer(
        fx.chan.clone(),
        stamp,
        Observation::Rows {
            rows: vec![Incoming::new(huge, Scope::Main)],
            scope: None,
            observed_ms: T0 + 1_000,
        },
    );
    assert_eq!(offered, Offered::Dropped);
    assert!(fx.cache.is_suspect(&fx.chan));
    fx.list(vec![m(1)], T0 + 500, Some(newest_proof(ListStart::At(T0))));
    assert!(fx.cache.is_suspect(&fx.chan), "it started before the drop");
    fx.list(vec![m(1)], T0 + 2_000, None);
    assert!(fx.cache.is_suspect(&fx.chan), "no proof of the newest end");
    fx.list(
        vec![m(1)],
        T0 + 2_000,
        Some(Proof {
            from: ListStart::At(T0),
            until: ListEnd::Before(T0 + STEP_MS),
            complete: true,
            full: true,
            source: coverage::Source::Range,
        }),
    );
    assert!(
        fx.cache.is_suspect(&fx.chan),
        "not a read of the newest end"
    );
    fx.list(
        vec![m(1)],
        T0 + 2_000,
        Some(newest_proof(ListStart::At(T0))),
    );
    assert!(!fx.cache.is_suspect(&fx.chan));
}

const STEP_MS: i64 = super::tests::STEP;

/// A batch is published to barrier waiters as soon as it commits: a control operation queued
/// behind it, here a pause standing for a long erase, does not hold it back.
#[test]
fn a_batch_is_published_before_the_control_operation_behind_it_runs() {
    let fx = Fixture::new("cache-race-publish");
    let other = ChanKey::new("p", "C2");
    let first = fx.cache.pause_writer();
    let stamp = fx.cache.stamp(&other);
    let Offered::Queued(seq) = fx.cache.offer(
        other.clone(),
        stamp,
        Observation::Rows {
            rows: vec![Incoming::new(m(1), Scope::Main)],
            scope: None,
            observed_ms: T0,
        },
    ) else {
        panic!("queued");
    };
    let (paused, stopped) = mpsc::channel();
    let (resume, resumed) = mpsc::channel::<()>();
    fx.cache
        .control(Op::Pause {
            paused,
            resume: resumed,
        })
        .expect("queued");
    // Both now reach the writer together: the offer opens a batch, and the pause ends it.
    drop(first);
    stopped.recv_timeout(SECONDS).expect("the second pause ran");
    assert!(
        fx.cache.applied_seq() >= seq,
        "the offer was held back by what came after it"
    );
    assert_eq!(
        fx.cache
            .newest(&other, &Scope::Main, 10)
            .served()
            .expect("served")
            .len(),
        1
    );
    drop(resume);
    fx.settle();
}

/// An erase is applied — its rows unreachable — at once, and a barrier for an offer queued after
/// it waits for that, not for the rows to be deleted, which happens in steps between batches.
#[test]
fn a_barrier_does_not_wait_for_an_erased_channels_rows_to_be_deleted() {
    let fx = Fixture::new("cache-race-erase-steps");
    let other = ChanKey::new("p", "C2");
    for chunk in 0..6 {
        fx.list(
            (chunk * 250..(chunk + 1) * 250)
                .map(|i| message(&format!("m{i}"), "ada", &format!("words {i}"), T0 + i))
                .collect(),
            T0,
            None,
        );
    }
    fx.cache.erase_channels(std::slice::from_ref(&fx.chan));
    let stamp = fx.cache.stamp(&other);
    fx.cache.offer(
        other.clone(),
        stamp,
        Observation::Rows {
            rows: vec![Incoming::new(m(1), Scope::Main)],
            scope: None,
            observed_ms: T0,
        },
    );
    assert!(fx.cache.barrier_blocking(SECONDS));
    let erased = fx.cache.stats().erased_rows;
    assert!(
        erased < 1_500,
        "the barrier waited for the deletions: {erased}"
    );
    assert!(fx.cache.background_busy());
    assert_eq!(
        fx.cache.newest(&fx.chan, &Scope::Main, 10_000),
        Read::Served(Vec::new())
    );
    assert!(fx
        .cache
        .message(&fx.chan, &mid("m7"))
        .served()
        .expect("served")
        .is_none());
    fx.settle();
    assert_eq!(fx.cache.stats().erased_rows, 1_500);
    assert_eq!(fx.cache.counts().served().expect("counts").messages, 1);
    assert_eq!(
        super::tests::sql_count(&fx.cache, "SELECT COUNT(*) FROM channel"),
        1,
        "the erased channel's row went with its last message"
    );
    super::tests::integrity(&fx);
}

/// Two file erases at once — the owner's "off" racing a purge — run one after the other: the
/// one that started second finds the file the first reopened, and erases it too.
#[test]
fn two_file_erases_at_once_run_one_after_the_other() {
    let fx = Fixture::new("cache-race-two-erases");
    fx.list(vec![m(1), m(2)], T0, None);
    let path = fx.path();
    let Fixture {
        cache, chan, dir, ..
    } = fx;
    let cache = Arc::new(cache);
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let reader = {
        let cache = Arc::clone(&cache);
        let chan = chan.clone();
        std::thread::spawn(move || {
            cache.read(Some(&chan), |c| {
                let _ = started_tx.send(());
                let _ = go_rx.recv_timeout(SECONDS);
                c.query_row("SELECT COUNT(*) FROM message", [], |r| r.get::<_, i64>(0))
            })
        })
    };
    started_rx.recv_timeout(SECONDS).expect("the read began");
    let purge = {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || cache.erase_file(true))
    };
    // The purge has taken the file, and waits for the read.
    while cache.is_open() {
        std::thread::sleep(Duration::from_millis(1));
    }
    let off = {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || cache.erase_file(false))
    };
    std::thread::sleep(Duration::from_millis(50));
    go_tx.send(()).expect("go");
    assert_eq!(reader.join().expect("reader"), Read::Moved);
    assert!(purge.join().expect("purge").expect("erased").reopened);
    assert!(!off.join().expect("off").expect("erased").reopened);
    assert!(!cache.is_open(), "off ran last, and left nothing open");
    assert!(!path.exists());
    assert_eq!(
        cache.offer(
            chan.clone(),
            cache.stamp(&chan),
            Observation::Deleted {
                ids: Vec::new(),
                observed_ms: T0,
            },
        ),
        Offered::Off
    );
    assert_eq!(cache.newest(&chan, &Scope::Main, 10), Read::Off);
    drop(dir);
}

/// A file erase discards whatever control operations were queued before it: they were for the
/// file being erased, and running them would only delay the erase.
#[test]
fn a_file_erase_discards_the_control_operations_queued_before_it() {
    let fx = Fixture::new("cache-race-discard");
    fx.list((0..10).map(m).collect(), T0, None);
    let Fixture {
        cache, chan, dir, ..
    } = fx;
    let cache = Arc::new(cache);
    let resume = cache.pause_writer();
    cache.maintain(Maintenance::Hourly);
    cache.erase_channels(std::slice::from_ref(&chan));
    let before = cache.stats();
    let eraser = {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || cache.erase_file(true))
    };
    while cache.is_open() {
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(resume);
    assert!(eraser.join().expect("eraser").expect("erased").reopened);
    let after = cache.stats();
    assert_eq!(
        after.ops_applied,
        before.ops_applied + 1,
        "only the pause ran"
    );
    assert_eq!(after.erased_channels, before.erased_channels);
    assert_eq!(after.expired_rows, before.expired_rows);
    drop(dir);
}
