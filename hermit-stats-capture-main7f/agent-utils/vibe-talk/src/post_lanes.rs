//! One post at a time to each destination, in the order the posts arrived. `#277 send-order`.
//!
//! The owner: "I write A then B but B lands first." The page now keeps each destination's messages
//! in the order they were written and sends one only once the one before it is confirmed. This
//! server is the last place that order can still be lost before the chat service, and it could be
//! lost here three ways:
//!
//! * **A retry overlapping its own first attempt.** The page gives up waiting for an answer after a
//!   minute, or signs out, or loses its connection behind a proxy, but the post it gave up on is
//!   still on its way to a slow bridge. The page's retry of A (same idempotency key) and then B
//!   would reach the bridge while that first A was still in flight, and B could land before it.
//!   Holding the lane in the request is not enough for this one: axum drops a request's handler
//!   the moment its caller disconnects, and whatever the handler holds goes with it. So the post
//!   is made by a task of its own that owns the turn ([`crate::ops::reply_scoped`]), and the
//!   destination stays held until the provider has answered, whether or not anyone still waits
//!   for the answer.
//! * **Two requests woken together.** Posts that find a rate-limit bucket empty all sleep until the
//!   same moment ([`crate::discord::ratelimit`]) and then race.
//! * **A post cut in parts.** Another post to the same place could land between its parts.
//!
//! So [`crate::ops::reply_scoped`] — every post this server makes goes through it — holds its
//! destination's lane from its first part to its last. Lanes are queued in the order the posts
//! arrived (tokio's mutex is fair: first come, first served). A destination is the channel and the
//! thread in it or none, so a slow thread holds up nothing anywhere else. A post still WAITING for
//! its turn gives up its place if its request goes away, which is safe because nothing of it has
//! been sent; a post that has started does not.
//!
//! Waiting is bounded by the post ahead of it, which is bounded by the provider client's own
//! request timeout and rate-limit budget: this adds no wait of its own and no timeout of its own.
//!
//! That bound is also the limit of what this can promise. When the provider client gives up on a
//! post — its own request timeout, `request_timeout_seconds`, 20 s by default — the call ends and
//! the destination is released, exactly as a disconnect used to release it, while a slow bridge
//! may still be working on the post. The next post to the same place can then reach the bridge
//! first. Past that point only the bridge can keep the order; the page's retry of the post, under
//! the same key, is still posted at most once there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};

/// A destination: the channel, and the thread in it or none.
type Lane = (String, Option<String>);

/// Every destination with a post in flight or waiting, each with its turn.
///
/// Only the lanes somebody holds or waits for are kept: a lane is dropped once nobody refers to it,
/// so this is as large as what is in flight, not as large as everything ever posted.
#[derive(Debug, Default)]
pub struct PostLanes {
    lanes: Mutex<HashMap<Lane, Weak<tokio::sync::Mutex<()>>>>,
}

/// A destination's turn, held until it is dropped.
pub type Turn = tokio::sync::OwnedMutexGuard<()>;

impl PostLanes {
    /// No destination in use.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wait for `channel`'s (and `thread`'s) turn, after every post to the same destination that
    /// asked before this one, and hold it until the [`Turn`] is dropped.
    pub async fn turn(&self, channel: &str, thread: Option<&str>) -> Turn {
        self.lane(channel, thread).lock_owned().await
    }

    /// How many destinations have a post in flight or waiting.
    #[must_use]
    pub fn in_use(&self) -> usize {
        self.lock()
            .values()
            .filter(|lane| lane.strong_count() > 0)
            .count()
    }

    fn lane(&self, channel: &str, thread: Option<&str>) -> Arc<tokio::sync::Mutex<()>> {
        let mut lanes = self.lock();
        let key = (channel.to_owned(), thread.map(str::to_owned));
        if let Some(lane) = lanes.get(&key).and_then(Weak::upgrade) {
            return lane;
        }
        lanes.retain(|_, lane| lane.strong_count() > 0);
        let lane = Arc::new(tokio::sync::Mutex::new(()));
        lanes.insert(key, Arc::downgrade(&lane));
        lane
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Lane, Weak<tokio::sync::Mutex<()>>>> {
        self.lanes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Let every task that can run, run.
    async fn quiet() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn posts_to_one_destination_take_their_turns_in_the_order_they_asked() {
        let lanes = Arc::new(PostLanes::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let first = lanes.turn("C1", None).await;
        let mut waiting = Vec::new();
        for name in ["B", "C", "D"] {
            let (lanes, order) = (Arc::clone(&lanes), Arc::clone(&order));
            waiting.push(tokio::spawn(async move {
                let _turn = lanes.turn("C1", None).await;
                order.lock().expect("order").push(name);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }));
            // Each asks after the one before it.
            quiet().await;
        }
        assert!(
            order.lock().expect("order").is_empty(),
            "a post went while the one ahead held the destination"
        );
        drop(first);
        for task in waiting {
            task.await.expect("the post finished");
        }
        assert_eq!(*order.lock().expect("order"), ["B", "C", "D"]);
        assert_eq!(lanes.in_use(), 0, "a finished destination was kept");
    }

    #[tokio::test]
    async fn another_thread_or_channel_does_not_wait() {
        let lanes = PostLanes::new();
        let _main = lanes.turn("C1", None).await;
        for (channel, thread) in [("C1", Some("T1")), ("C1", Some("T2")), ("C2", None)] {
            tokio::time::timeout(Duration::from_secs(1), lanes.turn(channel, thread))
                .await
                .unwrap_or_else(|_| panic!("{channel} {thread:?} waited for C1's main channel"));
        }
    }

    #[tokio::test]
    async fn a_post_whose_request_went_away_gives_up_its_place() {
        let lanes = Arc::new(PostLanes::new());
        let first = lanes.turn("C1", None).await;
        let gone = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move {
                let _turn = lanes.turn("C1", None).await;
            })
        };
        quiet().await;
        gone.abort();
        drop(first);
        tokio::time::timeout(Duration::from_secs(1), lanes.turn("C1", None))
            .await
            .expect("an abandoned wait kept the destination");
    }
}
