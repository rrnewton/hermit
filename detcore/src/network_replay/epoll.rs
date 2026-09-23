/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Epoll interest and delivery state, independent of the host and scheduler.
//!
//! Linux keys an interest by BOTH the numeric descriptor and its open file
//! description. Notifications are not merely false-to-true level transitions:
//! another arrival may notify an edge-triggered interest that is still readable.
//! The engine supplies exact levels and monotonic per-bit notification counters
//! at its deterministic observation boundary.
//!
//! A prepared batch reserves one instance without consuming its events. The
//! adapter must copy complete events in order, then commit only that prefix (or
//! cancel). Linux returns a successful prefix on a later copy fault. The lease
//! also detects concurrent control/second-wait misuse; it is NOT itself a
//! scheduler. The caller must serialize those operations or wait for the lease,
//! and release it on guest cancellation/exit. Never turn `DeliveryInProgress`
//! into a guest errno or hold an engine mutex across guest-memory awaits.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt;

use detcore_model::fd::OpenFileId;
use serde::Deserialize;
use serde::Serialize;

/// Readiness bits, in the order used by [`ReadinessSnapshot::notifications`].
pub(crate) const READINESS_BITS: [u32; 10] = [
    libc::EPOLLIN as u32,
    libc::EPOLLPRI as u32,
    libc::EPOLLOUT as u32,
    libc::EPOLLERR as u32,
    libc::EPOLLHUP as u32,
    libc::EPOLLRDNORM as u32,
    libc::EPOLLRDBAND as u32,
    libc::EPOLLWRNORM as u32,
    libc::EPOLLWRBAND as u32,
    libc::EPOLLRDHUP as u32,
];

const READINESS_MASK: u32 = (libc::EPOLLIN
    | libc::EPOLLPRI
    | libc::EPOLLOUT
    | libc::EPOLLERR
    | libc::EPOLLHUP
    | libc::EPOLLRDNORM
    | libc::EPOLLRDBAND
    | libc::EPOLLWRNORM
    | libc::EPOLLWRBAND
    | libc::EPOLLRDHUP) as u32;
const UNCONDITIONAL: u32 = (libc::EPOLLERR | libc::EPOLLHUP) as u32;

/// The descriptor table is deliberately absent: fork retains the same key.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
pub(crate) struct EpollKey {
    pub fd: i32,
    pub target: OpenFileId,
}

/// Exact poll levels and separate callback counters. In particular, receiving
/// EOF sets IN/RDHUP, not necessarily HUP. Counters may advance while the level
/// remains true, and must never wrap or decrease during an OFD's lifetime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReadinessSnapshot {
    pub events: u32,
    pub notifications: [u64; READINESS_BITS.len()],
}

impl ReadinessSnapshot {
    /// Record one callback for the specified bits. Counter exhaustion is an
    /// error with no partial mutation; it cannot masquerade as an old event.
    pub fn notify(&mut self, events: u32) -> Result<(), EpollError> {
        if events & !READINESS_MASK != 0 {
            return Err(EpollError::InvalidReadiness(events));
        }
        let mut updated = self.notifications;
        for (index, bit) in READINESS_BITS.iter().enumerate() {
            if events & bit != 0 {
                updated[index] = updated[index]
                    .checked_add(1)
                    .ok_or(EpollError::CounterExhausted)?;
            }
        }
        self.notifications = updated;
        Ok(())
    }

    fn changed_notifications(self, old: Self) -> Result<u32, EpollError> {
        if self.events & !READINESS_MASK != 0 {
            return Err(EpollError::InvalidReadiness(self.events));
        }
        let mut changed = self.events & !old.events;
        for (index, bit) in READINESS_BITS.iter().enumerate() {
            if self.notifications[index] < old.notifications[index] {
                return Err(EpollError::CounterWentBackwards);
            }
            if self.notifications[index] > old.notifications[index] {
                changed |= bit;
            }
        }
        Ok(changed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EpollError {
    UnknownInstance(OpenFileId),
    InstanceExists(OpenFileId),
    InterestExists(EpollKey),
    MissingInterest(EpollKey),
    SelfRegistration,
    ExclusiveUnsupported,
    InvalidReadiness(u32),
    InvalidMaxEvents,
    CounterWentBackwards,
    CounterExhausted,
    DeliveryInProgress,
    StaleDelivery,
    InvalidCopiedPrefix,
}

impl fmt::Display for EpollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "network epoll state: {self:?}")
    }
}

impl std::error::Error for EpollError {}

#[derive(Debug, Clone)]
struct Interest {
    mask: u32,
    data: u64,
    revision: u64,
    notification: u64,
    enabled: bool,
}

impl Interest {
    fn events(&self, snapshot: ReadinessSnapshot) -> u32 {
        if self.enabled {
            snapshot.events & (self.mask | UNCONDITIONAL)
        } else {
            0
        }
    }
}

#[derive(Debug, Clone)]
struct Instance {
    revision: u64,
    interests: BTreeMap<EpollKey, Interest>,
    ready: VecDeque<EpollKey>,
    delivery: Option<u64>,
}

impl Instance {
    fn enqueue(&mut self, key: EpollKey) {
        if !self.ready.contains(&key) {
            self.ready.push_back(key);
        }
    }

    fn idle(&self) -> Result<(), EpollError> {
        if self.delivery.is_some() {
            Err(EpollError::DeliveryInProgress)
        } else {
            Ok(())
        }
    }
}

/// One event plus private delivery receipts. Only `events` and `data` are
/// written to the guest; neither `key` nor a receipt substitutes for user data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PreparedEvent {
    pub key: EpollKey,
    pub events: u32,
    pub data: u64,
    revision: u64,
    notification: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PreparedEvents {
    instance: OpenFileId,
    instance_revision: u64,
    delivery: u64,
    events: Vec<PreparedEvent>,
}

impl PreparedEvents {
    pub fn events(&self) -> &[PreparedEvent] {
        &self.events
    }
}

/// State is shared across aliases and processes. The engine retains this
/// object for the run; a final OFD close must call `retire_open_file`.
#[derive(Debug, Default)]
pub(crate) struct EpollState {
    instances: BTreeMap<OpenFileId, Instance>,
    readiness: BTreeMap<OpenFileId, ReadinessSnapshot>,
    revision: u64,
}

impl EpollState {
    fn next_revision(&mut self) -> Result<u64, EpollError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(EpollError::CounterExhausted)?;
        Ok(self.revision)
    }

    pub fn create(&mut self, epoll: OpenFileId) -> Result<(), EpollError> {
        if self.instances.contains_key(&epoll) {
            return Err(EpollError::InstanceExists(epoll));
        }
        let revision = self.next_revision()?;
        self.instances.insert(
            epoll,
            Instance {
                revision,
                interests: BTreeMap::new(),
                ready: VecDeque::new(),
                delivery: None,
            },
        );
        Ok(())
    }

    fn instance(&self, epoll: OpenFileId) -> Result<&Instance, EpollError> {
        self.instances
            .get(&epoll)
            .ok_or(EpollError::UnknownInstance(epoll))
    }

    fn validate_interest(epoll: OpenFileId, key: EpollKey, mask: u32) -> Result<(), EpollError> {
        if epoll == key.target {
            return Err(EpollError::SelfRegistration);
        }
        if mask & libc::EPOLLEXCLUSIVE as u32 != 0 {
            return Err(EpollError::ExclusiveUnsupported);
        }
        // Other unused bits do not produce events. Linux ignores them. WAKEUP
        // has no guest autosleep service here; this module does not promise one.
        Ok(())
    }

    /// Descriptor/type/nesting validation belongs to the syscall adapter. This
    /// layer accepts only a validated fd/OFD identity, not a raw host descriptor.
    pub fn add(
        &mut self,
        epoll: OpenFileId,
        key: EpollKey,
        mask: u32,
        data: u64,
    ) -> Result<(), EpollError> {
        Self::validate_interest(epoll, key, mask)?;
        let instance = self.instance(epoll)?;
        instance.idle()?;
        if instance.interests.contains_key(&key) {
            return Err(EpollError::InterestExists(key));
        }
        let revision = self.next_revision()?;
        let snapshot = self.readiness.get(&key.target).copied().unwrap_or_default();
        let interest = Interest {
            mask,
            data,
            revision,
            notification: revision,
            enabled: true,
        };
        let ready = interest.events(snapshot) != 0;
        let instance = self.instances.get_mut(&epoll).unwrap();
        instance.interests.insert(key, interest);
        if ready {
            instance.enqueue(key);
        }
        Ok(())
    }

    /// MOD replaces data and rearms from current readiness, including ONESHOT.
    pub fn modify(
        &mut self,
        epoll: OpenFileId,
        key: EpollKey,
        mask: u32,
        data: u64,
    ) -> Result<(), EpollError> {
        Self::validate_interest(epoll, key, mask)?;
        let instance = self.instance(epoll)?;
        instance.idle()?;
        if !instance.interests.contains_key(&key) {
            return Err(EpollError::MissingInterest(key));
        }
        let revision = self.next_revision()?;
        let snapshot = self.readiness.get(&key.target).copied().unwrap_or_default();
        let instance = self.instances.get_mut(&epoll).unwrap();
        let interest = Interest {
            mask,
            data,
            revision,
            notification: revision,
            enabled: true,
        };
        let ready = interest.events(snapshot) != 0;
        instance.interests.insert(key, interest);
        // If it was already linked, Linux leaves its queue position intact.
        if ready {
            instance.enqueue(key);
        }
        Ok(())
    }

    pub fn delete(&mut self, epoll: OpenFileId, key: EpollKey) -> Result<(), EpollError> {
        let instance = self.instance(epoll)?;
        instance.idle()?;
        let instance = self.instances.get_mut(&epoll).unwrap();
        if instance.interests.remove(&key).is_none() {
            return Err(EpollError::MissingInterest(key));
        }
        instance.ready.retain(|candidate| *candidate != key);
        // An empty instance remains a valid wait target.
        Ok(())
    }

    /// Publish exact readiness. A matching notification can enqueue an ET
    /// interest even when no readiness level changed. Multiple pending callback
    /// notifications coalesce; notifications during copyout survive ET commit.
    pub fn update_readiness(
        &mut self,
        target: OpenFileId,
        snapshot: ReadinessSnapshot,
    ) -> Result<(), EpollError> {
        let old = self.readiness.get(&target).copied().unwrap_or_default();
        let changed = snapshot.changed_notifications(old)?;
        let revision = if changed != 0 {
            self.next_revision()?
        } else {
            self.revision
        };
        self.readiness.insert(target, snapshot);
        for instance in self.instances.values_mut() {
            let mut notify = Vec::new();
            for (key, interest) in &mut instance.interests {
                if key.target == target
                    && interest.enabled
                    && changed & (interest.mask | UNCONDITIONAL) != 0
                {
                    interest.notification = revision;
                    notify.push(*key);
                }
            }
            for key in notify {
                instance.enqueue(key);
            }
        }
        Ok(())
    }

    /// Nonconsuming scheduler predicate. Disabled oneshots and stale queue
    /// entries are not readiness; an empty instance is valid and returns false.
    pub fn has_ready(&self, epoll: OpenFileId) -> Result<bool, EpollError> {
        let instance = self.instance(epoll)?;
        // A prepared batch owns the instance through guest copyout. A second
        // waiter becomes eligible only after commit/cancel releases it.
        if instance.delivery.is_some() {
            return Ok(false);
        }
        Ok(instance.ready.iter().any(|key| {
            instance.interests.get(key).is_some_and(|interest| {
                interest.events(self.readiness.get(&key.target).copied().unwrap_or_default()) != 0
            })
        }))
    }

    pub fn prepare(
        &mut self,
        epoll: OpenFileId,
        maxevents: usize,
    ) -> Result<PreparedEvents, EpollError> {
        // This is Linux's EP_MAX_EVENTS, not an implementation-specific cap.
        if maxevents == 0 || maxevents > i32::MAX as usize / size_of::<libc::epoll_event>() {
            return Err(EpollError::InvalidMaxEvents);
        }
        self.instance(epoll)?.idle()?;
        let delivery = self.next_revision()?;
        let instance = self.instances.get_mut(&epoll).unwrap();
        let mut events = Vec::new();
        let mut stale = Vec::new();
        for key in &instance.ready {
            if events.len() == maxevents {
                break;
            }
            let Some(interest) = instance.interests.get(key) else {
                stale.push(*key);
                continue;
            };
            let mask =
                interest.events(self.readiness.get(&key.target).copied().unwrap_or_default());
            if mask == 0 {
                stale.push(*key);
                continue;
            }
            events.push(PreparedEvent {
                key: *key,
                events: mask,
                data: interest.data,
                revision: interest.revision,
                notification: interest.notification,
            });
        }
        instance.ready.retain(|key| !stale.contains(key));
        instance.delivery = Some(delivery);
        Ok(PreparedEvents {
            instance: epoll,
            instance_revision: instance.revision,
            delivery,
            events,
        })
    }

    fn validate_delivery(&self, batch: &PreparedEvents) -> Result<(), EpollError> {
        let instance = self
            .instances
            .get(&batch.instance)
            .ok_or(EpollError::StaleDelivery)?;
        if instance.revision != batch.instance_revision
            || instance.delivery != Some(batch.delivery)
            || batch.events.iter().any(|event| {
                instance.interests.get(&event.key).is_none_or(|interest| {
                    interest.revision != event.revision || interest.data != event.data
                })
            })
        {
            return Err(EpollError::StaleDelivery);
        }
        Ok(())
    }

    /// Commit only completely copied entries. A zero-prefix copy fault is
    /// commit(batch, 0) followed by guest EFAULT; a later fault returns the count.
    pub fn commit(&mut self, batch: PreparedEvents, copied: usize) -> Result<(), EpollError> {
        self.validate_delivery(&batch)?;
        if copied > batch.events.len() {
            return Err(EpollError::InvalidCopiedPrefix);
        }
        let instance = self.instances.get_mut(&batch.instance).unwrap();
        for event in batch.events.iter().take(copied) {
            instance.ready.retain(|key| *key != event.key);
            let interest = instance.interests.get_mut(&event.key).unwrap();
            if interest.mask & libc::EPOLLONESHOT as u32 != 0 {
                interest.enabled = false;
            } else if interest.mask & libc::EPOLLET as u32 == 0
                || interest.notification != event.notification
            {
                // LT rotation is behind every unselected/uncopied entry. ET
                // requeues only a notification newer than this prepared batch.
                instance.enqueue(event.key);
            }
        }
        instance.delivery = None;
        Ok(())
    }

    pub fn cancel(&mut self, batch: PreparedEvents) -> Result<(), EpollError> {
        self.validate_delivery(&batch)?;
        self.instances.get_mut(&batch.instance).unwrap().delivery = None;
        Ok(())
    }

    /// Call only for the FINAL open-file-description alias. Closing one dup or
    /// one forked descriptor must not remove its still-live registrations.
    /// Retirement invalidates any affected in-flight delivery; it cannot commit
    /// against a replacement, even if a caller erroneously reuses the identity.
    pub fn retire_open_file(&mut self, target: OpenFileId) {
        self.instances.remove(&target);
        self.readiness.remove(&target);
        for instance in self.instances.values_mut() {
            let old_len = instance.interests.len();
            instance.interests.retain(|key, _| key.target != target);
            instance.ready.retain(|key| key.target != target);
            if instance.interests.len() != old_len {
                instance.delivery = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::pid::DetTid;

    use super::*;

    const IN: u32 = libc::EPOLLIN as u32;
    const ET: u32 = libc::EPOLLET as u32;
    const ONESHOT: u32 = libc::EPOLLONESHOT as u32;

    fn ofd(sequence: u64) -> OpenFileId {
        OpenFileId::new(DetTid::from_raw(1), sequence)
    }

    fn key(fd: i32, sequence: u64) -> EpollKey {
        EpollKey {
            fd,
            target: ofd(sequence),
        }
    }

    fn setup() -> EpollState {
        let mut state = EpollState::default();
        state.create(ofd(0)).unwrap();
        state
    }

    fn ready(state: &mut EpollState, target: OpenFileId, events: u32) {
        let mut snapshot = state.readiness.get(&target).copied().unwrap_or_default();
        snapshot.events = events;
        snapshot.notify(events).unwrap();
        state.update_readiness(target, snapshot).unwrap();
    }

    fn take(state: &mut EpollState, maxevents: usize) -> Vec<(u32, u64)> {
        let batch = state.prepare(ofd(0), maxevents).unwrap();
        let events = batch
            .events()
            .iter()
            .map(|e| (e.events, e.data))
            .collect::<Vec<_>>();
        state.commit(batch, events.len()).unwrap();
        events
    }

    #[test]
    fn empty_instance_and_last_delete_remain_waitable() {
        let mut state = setup();
        assert_eq!(state.has_ready(ofd(0)), Ok(false));
        assert_eq!(take(&mut state, 1), []);
        state.add(ofd(0), key(3, 1), IN, 7).unwrap();
        state.delete(ofd(0), key(3, 1)).unwrap();
        assert_eq!(take(&mut state, 1), []);
        assert_eq!(
            state.delete(ofd(0), key(3, 1)),
            Err(EpollError::MissingInterest(key(3, 1)))
        );
    }

    #[test]
    fn fd_plus_ofd_key_preserves_dup_fork_and_reused_fd_semantics() {
        let mut state = setup();
        state.add(ofd(0), key(3, 1), IN | ET, 11).unwrap();
        state.add(ofd(0), key(4, 1), IN | ET, 22).unwrap();
        state.add(ofd(0), key(3, 2), IN | ET, 33).unwrap();
        // Same fd/OFD inherited through fork is the existing watch, not a new
        // descriptor-table-specific key. Opaque data need not be unique either.
        assert_eq!(
            state.add(ofd(0), key(3, 1), IN, 44),
            Err(EpollError::InterestExists(key(3, 1)))
        );
        ready(&mut state, ofd(1), IN);
        ready(&mut state, ofd(2), IN);
        assert_eq!(take(&mut state, 3), [(IN, 11), (IN, 22), (IN, 33)]);
        state.modify(ofd(0), key(4, 1), IN | ET, 11).unwrap();
        assert_eq!(take(&mut state, 3), [(IN, 11)]);
    }

    #[test]
    fn edge_arrival_while_readable_and_pending_coalescing() {
        let mut state = setup();
        state.add(ofd(0), key(3, 1), IN | ET, 9).unwrap();
        ready(&mut state, ofd(1), IN);
        ready(&mut state, ofd(1), IN);
        assert_eq!(take(&mut state, 1), [(IN, 9)]);
        assert_eq!(take(&mut state, 1), []);
        ready(&mut state, ofd(1), IN);
        assert_eq!(take(&mut state, 1), [(IN, 9)]);
    }

    #[test]
    fn snapshot_without_new_callback_does_not_repeat_an_edge() {
        let mut state = setup();
        ready(&mut state, ofd(1), IN);
        state.add(ofd(0), key(3, 1), IN | ET, 1).unwrap();
        assert_eq!(take(&mut state, 1), [(IN, 1)]);
        let same = state.readiness[&ofd(1)];
        state.update_readiness(ofd(1), same).unwrap();
        assert_eq!(state.has_ready(ofd(0)), Ok(false));
        assert_eq!(take(&mut state, 1), []);
    }

    #[test]
    fn one_shot_rearms_only_by_mod_and_replaces_data() {
        let mut state = setup();
        state.add(ofd(0), key(3, 1), IN | ONESHOT, 1).unwrap();
        ready(&mut state, ofd(1), IN);
        assert_eq!(take(&mut state, 1), [(IN, 1)]);
        ready(&mut state, ofd(1), IN);
        assert_eq!(state.has_ready(ofd(0)), Ok(false));
        state.modify(ofd(0), key(3, 1), IN | ONESHOT, 2).unwrap();
        assert_eq!(take(&mut state, 1), [(IN, 2)]);
        assert_eq!(take(&mut state, 1), []);
    }

    #[test]
    fn bounded_lt_delivery_rotates_without_starvation() {
        let mut state = setup();
        for n in 1..=3 {
            state.add(ofd(0), key(n as i32, n), IN, n).unwrap();
            ready(&mut state, ofd(n), IN);
        }
        let observed = (0..9).map(|_| take(&mut state, 1)[0].1).collect::<Vec<_>>();
        assert_eq!(observed, [1, 2, 3, 1, 2, 3, 1, 2, 3]);
    }

    #[test]
    fn maxevents_does_not_consume_unselected_edges_or_oneshots() {
        for mode in [ET, ONESHOT] {
            let mut state = setup();
            for n in 1..=3 {
                state.add(ofd(0), key(n as i32, n), IN | mode, n).unwrap();
                ready(&mut state, ofd(n), IN);
            }
            assert_eq!(take(&mut state, 1), [(IN, 1)]);
            assert_eq!(take(&mut state, 1), [(IN, 2)]);
            assert_eq!(take(&mut state, 1), [(IN, 3)]);
            assert_eq!(take(&mut state, 1), []);
        }
    }

    #[test]
    fn copy_fault_consumes_only_completed_prefix() {
        for mode in [ET, ONESHOT] {
            let mut state = setup();
            for n in 1..=3 {
                state.add(ofd(0), key(n as i32, n), IN | mode, n).unwrap();
                ready(&mut state, ofd(n), IN);
            }
            let batch = state.prepare(ofd(0), 3).unwrap();
            state.commit(batch, 0).unwrap();
            let batch = state.prepare(ofd(0), 3).unwrap();
            assert_eq!(batch.events.len(), 3);
            state.commit(batch, 1).unwrap();
            assert_eq!(take(&mut state, 3), [(IN, 2), (IN, 3)]);
            assert_eq!(take(&mut state, 3), []);
        }
    }

    #[test]
    fn cancel_leaves_events_and_delete_readd_rejects_old_receipt() {
        let mut state = setup();
        state.add(ofd(0), key(3, 1), IN | ONESHOT, 1).unwrap();
        ready(&mut state, ofd(1), IN);
        let old = state.prepare(ofd(0), 1).unwrap();
        state.cancel(old.clone()).unwrap();
        state.delete(ofd(0), key(3, 1)).unwrap();
        state.add(ofd(0), key(3, 1), IN | ONESHOT, 2).unwrap();
        let current = state.prepare(ofd(0), 1).unwrap();
        assert_eq!(state.commit(old, 1), Err(EpollError::StaleDelivery));
        assert_eq!(current.events[0].data, 2);
        state.cancel(current).unwrap();
        assert_eq!(take(&mut state, 1), [(IN, 2)]);
    }

    #[test]
    fn delivery_lease_detects_control_and_second_wait_interleaving() {
        let mut state = setup();
        state.add(ofd(0), key(3, 1), IN, 1).unwrap();
        ready(&mut state, ofd(1), IN);
        let batch = state.prepare(ofd(0), 1).unwrap();
        assert_eq!(
            state.prepare(ofd(0), 1),
            Err(EpollError::DeliveryInProgress)
        );
        assert_eq!(
            state.delete(ofd(0), key(3, 1)),
            Err(EpollError::DeliveryInProgress)
        );
        assert_eq!(
            state.modify(ofd(0), key(3, 1), IN, 2),
            Err(EpollError::DeliveryInProgress)
        );
        assert_eq!(
            state.add(ofd(0), key(4, 1), IN, 2),
            Err(EpollError::DeliveryInProgress)
        );
        state.cancel(batch).unwrap();
        state.delete(ofd(0), key(3, 1)).unwrap();
    }

    #[test]
    fn notification_during_copyout_survives_edge_commit() {
        let mut state = setup();
        state.add(ofd(0), key(3, 1), IN | ET, 1).unwrap();
        ready(&mut state, ofd(1), IN);
        let batch = state.prepare(ofd(0), 1).unwrap();
        ready(&mut state, ofd(1), IN);
        state.commit(batch, 1).unwrap();
        assert_eq!(take(&mut state, 1), [(IN, 1)]);
        assert_eq!(take(&mut state, 1), []);
    }

    #[test]
    fn notification_during_oneshot_copy_does_not_rearm_it() {
        let mut state = setup();
        state.add(ofd(0), key(3, 1), IN | ONESHOT, 1).unwrap();
        ready(&mut state, ofd(1), IN);
        let batch = state.prepare(ofd(0), 1).unwrap();
        ready(&mut state, ofd(1), IN);
        state.commit(batch, 1).unwrap();
        assert_eq!(take(&mut state, 1), []);
        state.modify(ofd(0), key(3, 1), IN | ONESHOT, 2).unwrap();
        assert_eq!(take(&mut state, 1), [(IN, 2)]);
    }

    #[test]
    fn full_hup_and_error_unrequested_but_read_hup_and_priority_filtered() {
        let mut state = setup();
        state.add(ofd(0), key(3, 1), IN, 1).unwrap();
        ready(
            &mut state,
            ofd(1),
            (libc::EPOLLRDHUP | libc::EPOLLPRI) as u32,
        );
        assert_eq!(take(&mut state, 1), []);
        state
            .modify(
                ofd(0),
                key(3, 1),
                (libc::EPOLLRDHUP | libc::EPOLLPRI) as u32,
                2,
            )
            .unwrap();
        assert_eq!(
            take(&mut state, 1),
            [((libc::EPOLLRDHUP | libc::EPOLLPRI) as u32, 2)]
        );
        ready(&mut state, ofd(1), UNCONDITIONAL);
        assert_eq!(take(&mut state, 1), [(UNCONDITIONAL, 2)]);
    }

    #[test]
    fn stale_level_is_not_returned_and_later_arrival_wakes_again() {
        let mut state = setup();
        state.add(ofd(0), key(3, 1), IN | ET, 1).unwrap();
        ready(&mut state, ofd(1), IN);
        ready(&mut state, ofd(1), 0);
        assert_eq!(state.has_ready(ofd(0)), Ok(false));
        assert_eq!(take(&mut state, 1), []);
        ready(&mut state, ofd(1), IN);
        assert_eq!(take(&mut state, 1), [(IN, 1)]);
    }

    #[test]
    fn final_ofd_retirement_removes_all_aliases_and_invalidates_delivery() {
        let mut state = setup();
        for fd in [3, 4] {
            state.add(ofd(0), key(fd, 1), IN, fd as u64).unwrap();
        }
        ready(&mut state, ofd(1), IN);
        let batch = state.prepare(ofd(0), 2).unwrap();
        state.retire_open_file(ofd(1));
        assert_eq!(state.commit(batch, 1), Err(EpollError::StaleDelivery));
        assert_eq!(take(&mut state, 2), []);
        let empty = state.prepare(ofd(0), 1).unwrap();
        state.retire_open_file(ofd(0));
        state.create(ofd(0)).unwrap();
        assert_eq!(state.cancel(empty), Err(EpollError::StaleDelivery));
    }

    #[test]
    fn invalid_bounds_masks_and_generations_fail_without_mutating_delivery() {
        let mut state = setup();
        assert_eq!(state.prepare(ofd(0), 0), Err(EpollError::InvalidMaxEvents));
        assert_eq!(
            state.prepare(ofd(0), usize::MAX),
            Err(EpollError::InvalidMaxEvents)
        );
        assert_eq!(
            state.add(ofd(0), key(3, 0), IN, 1),
            Err(EpollError::SelfRegistration)
        );
        assert_eq!(
            state.add(ofd(0), key(3, 1), libc::EPOLLEXCLUSIVE as u32, 1),
            Err(EpollError::ExclusiveUnsupported)
        );
        ready(&mut state, ofd(1), IN);
        let saved = state.readiness[&ofd(1)];
        assert_eq!(
            state.update_readiness(ofd(1), ReadinessSnapshot::default()),
            Err(EpollError::CounterWentBackwards)
        );
        assert_eq!(state.readiness[&ofd(1)], saved);
        let mut overflow = ReadinessSnapshot::default();
        overflow.notifications[2] = u64::MAX;
        let before = overflow;
        assert_eq!(
            overflow.notify(IN | libc::EPOLLOUT as u32),
            Err(EpollError::CounterExhausted)
        );
        assert_eq!(overflow, before);
        assert_eq!(overflow.notify(ET), Err(EpollError::InvalidReadiness(ET)));
    }

    #[test]
    fn active_delivery_is_ineligible_until_commit_or_cancel() {
        let mut state = setup();
        state.add(ofd(0), key(3, 1), IN, 7).unwrap();
        ready(&mut state, ofd(1), IN);
        assert_eq!(state.has_ready(ofd(0)), Ok(true));
        let first = state.prepare(ofd(0), 1).unwrap();
        assert_eq!(state.has_ready(ofd(0)), Ok(false));
        // An arrival during the copy does not permit a second waiter to enter.
        ready(&mut state, ofd(1), IN);
        assert_eq!(state.has_ready(ofd(0)), Ok(false));
        state.cancel(first).unwrap();
        assert_eq!(state.has_ready(ofd(0)), Ok(true));
        let second = state.prepare(ofd(0), 1).unwrap();
        state.commit(second, 0).unwrap();
        assert_eq!(state.has_ready(ofd(0)), Ok(true));
        let third = state.prepare(ofd(0), 1).unwrap();
        state.commit(third, 1).unwrap();
        assert_eq!(state.has_ready(ofd(0)), Ok(true)); // level-triggered rotation
    }

    // These Linux syscall differential tests do not execute Hermit. Waits are
    // nonblocking or bounded by one second; every descriptor is RAII-owned.
    struct NativeEpoll(std::os::fd::OwnedFd);

    impl NativeEpoll {
        fn new() -> Self {
            use std::os::fd::FromRawFd;
            let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
            assert!(fd >= 0, "{}", std::io::Error::last_os_error());
            Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
        }

        fn ctl(&self, op: i32, fd: i32, events: u32, data: u64) {
            use std::os::fd::AsRawFd;
            let mut event = libc::epoll_event { events, u64: data };
            assert_eq!(
                unsafe { libc::epoll_ctl(self.0.as_raw_fd(), op, fd, &mut event) },
                0,
                "{}",
                std::io::Error::last_os_error()
            );
        }

        fn wait(&self, maxevents: usize, timeout: i32) -> Vec<(u32, u64)> {
            use std::os::fd::AsRawFd;
            let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; maxevents];
            let count = unsafe {
                libc::epoll_wait(
                    self.0.as_raw_fd(),
                    events.as_mut_ptr(),
                    maxevents as i32,
                    timeout,
                )
            };
            assert!(count >= 0, "{}", std::io::Error::last_os_error());
            events.truncate(count as usize);
            events.into_iter().map(|e| (e.events, e.u64)).collect()
        }
    }

    #[test]
    fn linux_differential_dup_edge_arrival_while_unread() {
        use std::io::Read;
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer.set_nonblocking(true).unwrap();
        let alias = reader.try_clone().unwrap();
        let epoll = NativeEpoll::new();
        let mut state = setup();
        for (fd, data) in [(reader.as_raw_fd(), 11), (alias.as_raw_fd(), 22)] {
            epoll.ctl(libc::EPOLL_CTL_ADD, fd, IN | ET, data);
            state.add(ofd(0), key(fd, 1), IN | ET, data).unwrap();
        }
        for byte in [b'a', b'b'] {
            writer.write_all(&[byte]).unwrap();
            ready(&mut state, ofd(1), IN);
            // Linux does not specify dup callback traversal order. Compare the
            // complete set with exact masks/data, rather than one lucky order.
            let mut native = epoll.wait(2, 1000);
            let mut modeled = take(&mut state, 2);
            native.sort_unstable();
            modeled.sort_unstable();
            assert_eq!(native, [(IN, 11), (IN, 22)]);
            assert_eq!(modeled, native);
            assert_eq!(epoll.wait(2, 0), []);
            assert_eq!(take(&mut state, 2), []);
        }
        let mut bytes = [0; 2];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, *b"ab");
    }

    #[test]
    fn linux_differential_oneshot_mod_and_empty_after_delete() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let epoll = NativeEpoll::new();
        let mut state = setup();
        let fd = reader.as_raw_fd();
        epoll.ctl(libc::EPOLL_CTL_ADD, fd, IN | ONESHOT, 10);
        state.add(ofd(0), key(fd, 1), IN | ONESHOT, 10).unwrap();
        writer.write_all(b"x").unwrap();
        ready(&mut state, ofd(1), IN);
        assert_eq!(epoll.wait(1, 1000), [(IN, 10)]);
        assert_eq!(take(&mut state, 1), [(IN, 10)]);
        assert_eq!(epoll.wait(1, 0), []);
        assert_eq!(take(&mut state, 1), []);
        epoll.ctl(libc::EPOLL_CTL_MOD, fd, IN | ONESHOT, 20);
        state.modify(ofd(0), key(fd, 1), IN | ONESHOT, 20).unwrap();
        assert_eq!(epoll.wait(1, 1000), [(IN, 20)]);
        assert_eq!(take(&mut state, 1), [(IN, 20)]);
        epoll.ctl(libc::EPOLL_CTL_DEL, fd, 0, 0);
        state.delete(ofd(0), key(fd, 1)).unwrap();
        assert_eq!(epoll.wait(1, 0), []);
        assert_eq!(take(&mut state, 1), []);
    }

    #[test]
    fn linux_differential_lt_round_robin() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let epoll = NativeEpoll::new();
        let mut state = setup();
        let mut streams = Vec::new();
        for n in 1..=3 {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            writer.set_nonblocking(true).unwrap();
            epoll.ctl(libc::EPOLL_CTL_ADD, reader.as_raw_fd(), IN, n);
            state
                .add(ofd(0), key(reader.as_raw_fd(), n), IN, n)
                .unwrap();
            writer.write_all(b"x").unwrap();
            ready(&mut state, ofd(n), IN);
            streams.push((reader, writer));
        }
        for n in [1, 2, 3, 1, 2, 3] {
            assert_eq!(epoll.wait(1, 1000), [(IN, n)]);
            assert_eq!(take(&mut state, 1), [(IN, n)]);
        }
    }

    #[test]
    fn linux_differential_tcp_read_eof_is_not_full_hup() {
        use std::io::Read;
        use std::io::Write;
        use std::net::Shutdown;
        use std::net::TcpListener;
        use std::net::TcpStream;
        use std::os::fd::AsRawFd;
        use std::time::Duration;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut peer =
            TcpStream::connect_timeout(&listener.local_addr().unwrap(), Duration::from_secs(1))
                .unwrap();
        listener.set_nonblocking(true).unwrap();
        let (mut observer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        observer
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        observer
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let epoll = NativeEpoll::new();
        let mask = IN | libc::EPOLLRDHUP as u32;
        epoll.ctl(libc::EPOLL_CTL_ADD, observer.as_raw_fd(), mask, 7);
        let mut state = setup();
        state
            .add(ofd(0), key(observer.as_raw_fd(), 1), mask, 7)
            .unwrap();
        peer.shutdown(Shutdown::Write).unwrap();
        assert_eq!(epoll.wait(1, 1000), [(mask, 7)]);
        ready(&mut state, ofd(1), mask);
        assert_eq!(take(&mut state, 1), [(mask, 7)]);
        assert_eq!(observer.read(&mut [0u8; 1]).unwrap(), 0);
        observer.write_all(b"x").unwrap();
        let mut reverse = [0];
        peer.read_exact(&mut reverse).unwrap();
        assert_eq!(reverse, *b"x");
        observer.shutdown(Shutdown::Write).unwrap();
        let full = mask | libc::EPOLLHUP as u32;
        assert_eq!(epoll.wait(1, 1000), [(full, 7)]);
        ready(&mut state, ofd(1), full);
        assert_eq!(take(&mut state, 1), [(full, 7)]);
    }

    #[test]
    fn linux_differential_copy_fault_preserves_uncopied_oneshot() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        struct Pages(*mut libc::c_void, usize);
        impl Drop for Pages {
            fn drop(&mut self) {
                assert_eq!(unsafe { libc::munmap(self.0, self.1) }, 0);
            }
        }
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(page_size > 0);
        let page_size = page_size as usize;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page_size * 2,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        let pages = Pages(ptr, page_size * 2);
        let inaccessible = unsafe { ptr.cast::<u8>().add(page_size) };
        assert_eq!(
            unsafe { libc::mprotect(inaccessible.cast(), page_size, libc::PROT_NONE) },
            0
        );
        let epoll = NativeEpoll::new();
        let mut state = setup();
        let mut streams = Vec::new();
        for n in 1..=2 {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            writer.set_nonblocking(true).unwrap();
            epoll.ctl(libc::EPOLL_CTL_ADD, reader.as_raw_fd(), IN | ONESHOT, n);
            state
                .add(ofd(0), key(reader.as_raw_fd(), n), IN | ONESHOT, n)
                .unwrap();
            writer.write_all(b"x").unwrap();
            ready(&mut state, ofd(n), IN);
            streams.push((reader, writer));
        }
        assert_eq!(
            unsafe { libc::epoll_wait(epoll.0.as_raw_fd(), inaccessible.cast(), 2, 0) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EFAULT)
        );
        let batch = state.prepare(ofd(0), 2).unwrap();
        assert_eq!(batch.events.len(), 2);
        state.commit(batch, 0).unwrap();

        let partial = unsafe {
            inaccessible
                .sub(size_of::<libc::epoll_event>())
                .cast::<libc::epoll_event>()
        };
        assert_eq!(
            unsafe { libc::epoll_wait(epoll.0.as_raw_fd(), partial, 2, 0) },
            1
        );
        let first = unsafe { std::ptr::read_unaligned(partial) };
        let first = (first.events, first.u64);
        let batch = state.prepare(ofd(0), 2).unwrap();
        assert_eq!(first, (batch.events[0].events, batch.events[0].data));
        state.commit(batch, 1).unwrap();
        assert_eq!(first, (IN, 1));
        assert_eq!(epoll.wait(2, 0), [(IN, 2)]);
        assert_eq!(take(&mut state, 2), [(IN, 2)]);
        assert_eq!(epoll.wait(2, 0), []);
        assert_eq!(take(&mut state, 2), []);
        drop(pages);
    }
}
