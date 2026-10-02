/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A clonable wait on a oneshot receiver, for the Narf kernel build of
//! Detcore, whose `futures` has no `Shared` (it needs std). The host build
//! waits with `Shared` and compiles this module only for its tests.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::task::Wake;
use std::task::Waker;

use futures::channel::oneshot;

/// A wait on a `oneshot::Receiver<()>` that can be cloned, like
/// `futures::future::Shared` of the receiver: every clone resolves with the
/// receiver's outcome, and a clone made after the outcome resolves at once.
pub(crate) struct SharedReceiver {
    inner: Arc<Inner>,
    /// This clone's entry in `State::waiting`, from its first pending poll.
    key: Option<u64>,
}

struct Inner {
    state: Mutex<State>,
}

struct State {
    /// The receiver, until it yields its outcome.
    receiver: Option<oneshot::Receiver<()>>,
    outcome: Option<Result<(), oneshot::Canceled>>,
    /// The wakers of the clones that are waiting for the outcome.
    waiting: BTreeMap<u64, Waker>,
    next_key: u64,
}

impl SharedReceiver {
    /// Share the wait on `receiver`.
    pub(crate) fn new(receiver: oneshot::Receiver<()>) -> Self {
        SharedReceiver {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    receiver: Some(receiver),
                    outcome: None,
                    waiting: BTreeMap::new(),
                    next_key: 0,
                }),
            }),
            key: None,
        }
    }
}

impl std::fmt::Debug for SharedReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedReceiver")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl Clone for SharedReceiver {
    fn clone(&self) -> Self {
        SharedReceiver {
            inner: self.inner.clone(),
            key: None,
        }
    }
}

/// The receiver keeps only the waker of its latest poll, so it is polled with
/// this one, which wakes every waiting clone. A clone that polled last and was
/// then dropped cannot strand the others.
impl Wake for Inner {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let waiting = std::mem::take(&mut self.state.lock().unwrap().waiting);
        for waker in waiting.into_values() {
            waker.wake();
        }
    }
}

impl Future for SharedReceiver {
    type Output = Result<(), oneshot::Canceled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // Held until this clone's waker is registered, so a wake from the
        // sender cannot fall between the receiver's poll and the registration.
        let mut state = this.inner.state.lock().unwrap();
        if let Some(outcome) = state.outcome {
            return Poll::Ready(outcome);
        }
        let notifier = Waker::from(this.inner.clone());
        let receiver = state
            .receiver
            .as_mut()
            .expect("the receiver stays until its outcome is recorded");
        match Pin::new(receiver).poll(&mut Context::from_waker(&notifier)) {
            Poll::Pending => {
                let key = match this.key {
                    Some(key) => key,
                    None => {
                        let key = state.next_key;
                        state.next_key += 1;
                        this.key = Some(key);
                        key
                    }
                };
                state.waiting.insert(key, cx.waker().clone());
                Poll::Pending
            }
            Poll::Ready(outcome) => {
                state.receiver = None;
                state.outcome = Some(outcome);
                let waiting = std::mem::take(&mut state.waiting);
                drop(state);
                for waker in waiting.into_values() {
                    waker.wake();
                }
                Poll::Ready(outcome)
            }
        }
    }
}

impl Drop for SharedReceiver {
    fn drop(&mut self) {
        if let Some(key) = self.key
            && let Ok(mut state) = self.inner.state.lock()
        {
            state.waiting.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::*;

    /// Counts the wakes it receives.
    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (Arc<WakeCount>, Waker) {
        let count = Arc::new(WakeCount::default());
        let waker = Waker::from(count.clone());
        (count, waker)
    }

    fn wakes(count: &WakeCount) -> usize {
        count.0.load(Ordering::SeqCst)
    }

    fn poll(receiver: &mut SharedReceiver, waker: &Waker) -> Poll<Result<(), oneshot::Canceled>> {
        Pin::new(receiver).poll(&mut Context::from_waker(waker))
    }

    #[test]
    fn every_waiting_clone_wakes_and_resolves() {
        let (sender, receiver) = oneshot::channel();
        let mut first = SharedReceiver::new(receiver);
        let mut second = first.clone();
        let (first_count, first_waker) = counting_waker();
        let (second_count, second_waker) = counting_waker();
        assert!(poll(&mut first, &first_waker).is_pending());
        assert!(poll(&mut second, &second_waker).is_pending());
        sender.send(()).unwrap();
        assert_eq!(wakes(&first_count), 1);
        assert_eq!(wakes(&second_count), 1);
        assert_eq!(poll(&mut first, &first_waker), Poll::Ready(Ok(())));
        assert_eq!(poll(&mut second, &second_waker), Poll::Ready(Ok(())));
        assert_eq!(wakes(&first_count), 1);
        assert_eq!(wakes(&second_count), 1);
    }

    #[test]
    fn a_dropped_last_poller_does_not_strand_the_others() {
        let (sender, receiver) = oneshot::channel();
        let mut first = SharedReceiver::new(receiver);
        let mut second = first.clone();
        let (first_count, first_waker) = counting_waker();
        let (second_count, second_waker) = counting_waker();
        assert!(poll(&mut first, &first_waker).is_pending());
        // The receiver now holds the waker of this later poll.
        assert!(poll(&mut second, &second_waker).is_pending());
        drop(second);
        sender.send(()).unwrap();
        assert_eq!(wakes(&first_count), 1);
        assert_eq!(wakes(&second_count), 0);
        assert_eq!(poll(&mut first, &first_waker), Poll::Ready(Ok(())));
    }

    #[test]
    fn a_clone_made_after_the_outcome_resolves_at_once() {
        let (sender, receiver) = oneshot::channel();
        let mut first = SharedReceiver::new(receiver);
        let (count, waker) = counting_waker();
        sender.send(()).unwrap();
        assert_eq!(poll(&mut first, &waker), Poll::Ready(Ok(())));
        let mut late = first.clone();
        assert_eq!(poll(&mut late, &waker), Poll::Ready(Ok(())));
        assert_eq!(wakes(&count), 0);
    }

    #[test]
    fn a_dropped_sender_cancels_every_clone() {
        let (sender, receiver) = oneshot::channel::<()>();
        let mut first = SharedReceiver::new(receiver);
        let mut second = first.clone();
        let (first_count, first_waker) = counting_waker();
        let (_, second_waker) = counting_waker();
        assert!(poll(&mut first, &first_waker).is_pending());
        drop(sender);
        assert_eq!(wakes(&first_count), 1);
        assert_eq!(
            poll(&mut first, &first_waker),
            Poll::Ready(Err(oneshot::Canceled))
        );
        assert_eq!(
            poll(&mut second, &second_waker),
            Poll::Ready(Err(oneshot::Canceled))
        );
    }
}
