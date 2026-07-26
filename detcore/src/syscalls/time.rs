/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls for dealing with threads and concurrency.
use std::time::Duration;

use nix::sys::signal::Signal;
use procfs::process::Process;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::Timespec;
use reverie::syscalls::Timeval;
use reverie::syscalls::family::NanosleepFamily;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;

use crate::detlog;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::Priority;
use crate::scheduler::entropy_to_priority;
use crate::tool_global::ResumeStatus;
use crate::tool_global::resource_request;
use crate::tool_global::thread_observe_time;
use crate::tool_global::update_posix_timer;
use crate::tool_local::Detcore;
use crate::types::LogicalTime;

fn time_from_resources(rsrcs: &Resources) -> Option<LogicalTime> {
    if rsrcs.resources.len() > 1 {
        panic!(
            "time_from_resources: multiple resource ids in resource request: {:?}",
            rsrcs
        );
    }
    for rs in rsrcs.resources.iter() {
        if let (ResourceID::SleepUntil(tm), _) = rs {
            return Some(*tm);
        }
    }
    None
}

/// Validate and flatten a timer `timespec` to nanoseconds.
fn timespec_to_ns(ts: libc::timespec) -> Result<u64, Errno> {
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    Ok((ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64))
}

/// Inverse of [`timespec_to_ns`].
fn ns_to_timespec(ns: u64) -> libc::timespec {
    libc::timespec {
        tv_sec: (ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (ns % 1_000_000_000) as libc::c_long,
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Convenience function for constructing a sleep request with a nanosecond offset from "now".
    pub async fn sleep_request<G: Guest<Self>>(guest: &mut G, ns_delta: Duration) -> Resources {
        let base_time = thread_observe_time(guest).await;
        let target_time = base_time + ns_delta;
        let resource = ResourceID::SleepUntil(target_time);
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// Convenience function for constructing a sleep request with a absolute nanosecond value from the realtime clock.
    pub async fn sleep_request_abs<G: Guest<Self>>(guest: &mut G, time: LogicalTime) -> Resources {
        // TODO T124594597 Record-replay case requires better handling of time
        let resource = ResourceID::SleepUntil(time);
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// Convenience function for constructing a thread yield request.
    /// Implemented as a sleep ending at the epoch (in the past).
    pub fn yield_request<G: Guest<Self>>(guest: &mut G) -> Resources {
        let resource = ResourceID::SleepUntil(LogicalTime::from_nanos(0));
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// Construct a request for a strong, one-turn scheduler yield.
    pub fn sched_yield_request<G: Guest<Self>>(guest: &mut G) -> Resources {
        guest
            .thread_state()
            .mk_request(ResourceID::SchedYield, Permission::W)
    }

    /// Construct a random PriorityChangePoint request using the local PRNG.
    pub fn random_priority_changepoint_request<G: Guest<Self>>(
        guest: &mut G,
        change_time: LogicalTime,
    ) -> Resources {
        let entropy = guest.thread_state_mut().chaos_prng_next_u64("priority");
        let new_priority = entropy_to_priority(entropy);
        Self::priority_changepoint_request(guest, change_time, new_priority)
    }

    /// Construct a PriorityChangePoint request using the supplied time and priority.
    pub fn priority_changepoint_request<G: Guest<Self>>(
        guest: &mut G,
        change_time: LogicalTime,
        new_priority: Priority,
    ) -> Resources {
        let resource = ResourceID::PriorityChangePoint(new_priority, change_time);
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// gettimeofday
    pub async fn handle_gettimeofday<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Gettimeofday,
    ) -> Result<i64, Error> {
        let time_ns = thread_observe_time(guest).await;

        let ret = self.record_or_replay(guest, call).await?;

        let mut memory = guest.memory();

        let tv: Timeval = time_ns.into();

        if let Some(tp) = call.tv() {
            memory.write_value(tp, &tv)?;
        }

        Ok(ret)
    }

    /// time
    pub async fn handle_time<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Time,
    ) -> Result<i64, Error> {
        let time_ns = thread_observe_time(guest).await;
        let secs = time_ns.as_secs() as i64;

        if let Some(tloc) = call.tloc() {
            let mut memory = guest.memory();
            memory.write_value(tloc, &secs)?;
        }

        Ok(secs)
    }

    /// clock_gettime
    pub async fn handle_clock_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ClockGettime,
    ) -> Result<i64, Error> {
        let time_ns = thread_observe_time(guest).await;
        trace!("Converting nanoseconds into clock_gettime: {}", time_ns);

        let tp = call.tp().ok_or(Errno::EFAULT)?;

        let t: Timespec = time_ns.into();

        guest.memory().write_value(tp, &t)?;

        Ok(0)
    }

    /// clock_gettime
    pub async fn handle_clock_getres<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ClockGetres,
    ) -> Result<i64, Error> {
        let res = call.res().ok_or(Errno::EFAULT)?;

        // For now we report a constant clock res of 10ms:
        let clock_res = 10;

        let t = Timespec {
            tv_sec: 0,
            tv_nsec: 1000 * clock_res as i64,
        };

        guest.memory().write_value(res, &t)?;

        Ok(0)
    }

    /// Helper function to wait a given period, which may either succeed or be interrupted by a signal.
    /// Return 0 or EINTR respectively.
    async fn wait_and_return<R: Guest<Self>>(
        guest: &mut R,
        request: Resources,
        call: NanosleepFamily,
    ) -> Result<i64, Error> {
        let target_time = time_from_resources(&request).expect("a sleepuntil resource request");
        match resource_request(guest, request).await {
            ResumeStatus::Normal => Ok(0),
            ResumeStatus::Signaled => {
                let now = thread_observe_time(guest).await;
                let delta: Duration = target_time.duration_since(now);
                let addr2 = call.rem();
                if let Some(addr2) = addr2 {
                    info!(
                        "[interrupted] sleep till (until {}), woke up {:?} early, writing into nanosleep rem argument.",
                        target_time, delta
                    );
                    let t = Timespec {
                        tv_sec: delta.as_secs() as i64,
                        tv_nsec: delta.subsec_nanos() as i64,
                    };
                    guest.memory().write_value(addr2, &t)?;
                } else {
                    info!("[interrupted] nanosleep rem argument is null, not writing it.")
                }
                Err(reverie::Error::Errno(Errno::EINTR))
            }
        }
    }

    /// clock_nanosleep and nanosleep
    pub async fn handle_nanosleep_family<R: Guest<Self>>(
        &self,
        guest: &mut R,
        call: NanosleepFamily,
    ) -> Result<i64, Error> {
        if call.flags() > libc::TIMER_ABSTIME {
            trace!("Unhandled clock_nanosleep flags, letting syscall through...");
            return Ok(guest.inject(Syscall::from(call)).await?);
        }

        // TODO: use 2nd, `rem` argument when providing a way for a signal to interrupt the
        // logical sleep.
        let addr = call.req().ok_or(Errno::EFAULT)?;
        let t: Timespec = guest
            .memory()
            .read_value(addr)
            .expect("should be able to read from memory");

        match call.flags() {
            0 => {
                if self.cfg.sequentialize_threads {
                    let time = Duration::from_secs(t.tv_sec as u64)
                        + Duration::from_nanos(t.tv_nsec as u64);
                    let request = Self::sleep_request(guest, time).await;
                    trace!(
                        "nanosleep adding delta {:?} to yield request {:?}",
                        time, &request
                    );
                    Self::wait_and_return(guest, request, call).await
                } else {
                    trace!("Not sequentializing threads, letting nanosleep through...");
                    Ok(guest.inject(Syscall::from(call)).await?)
                }
            }
            libc::TIMER_ABSTIME => {
                let target_time = LogicalTime::from_secs(t.tv_sec as u64)
                    + LogicalTime::from_nanos(t.tv_nsec as u64);
                if self.cfg.sequentialize_threads {
                    if self.cfg.virtualize_time {
                        let request = Self::sleep_request_abs(guest, target_time).await;
                        trace!(
                            "nanosleep setting absolute time {:?} to yield request {:?}",
                            target_time, &request
                        );
                        Self::wait_and_return(guest, request, call).await
                    } else {
                        // TODO T124594597: Record-replay case here, need better ideas to enable proper handling of this case.
                        error!(
                            "Sequentializing but not virtualizing, so can't rely on passed abs time, especially when replaying a recording, just yelding"
                        );
                        let request = Self::yield_request(guest);
                        Self::wait_and_return(guest, request, call).await
                    }
                } else if self.cfg.virtualize_time {
                    trace!(
                        "Not sequentializing, but virtualizing so calculating relative time and invoking nanosleep..."
                    );
                    let relative_ts = Self::relative_time_from_abs_target(guest, target_time).await;
                    let mut stack = guest.stack().await;
                    let req = stack.push(relative_ts);
                    stack.commit()?;
                    let modified_call = syscalls::Nanosleep::new().with_req(Some(req));
                    Ok(guest.inject(modified_call).await?)
                } else {
                    trace!(
                        "Not sequentializing threads not virtualizing, letting nanosleep through..."
                    );
                    Ok(guest.inject(Syscall::from(call)).await?)
                }
            }
            _ => unreachable!("Unexpected, unhandled flag value"),
        }
    }

    async fn relative_time_from_abs_target<G: Guest<Self>>(
        guest: &mut G,
        target_time: LogicalTime,
    ) -> Timespec {
        let base_time = thread_observe_time(guest).await;
        let relative_logical = target_time - base_time;

        Timespec {
            tv_sec: relative_logical.as_secs() as i64,
            tv_nsec: relative_logical.subsec_nanos() as i64,
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-TBD): Review deterministic one-shot SIGEV_SIGNAL delivery.
    /// Allocate a deterministic process-local timer id and retain its notification mode.
    pub async fn handle_timer_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerCreate,
    ) -> Result<i64, Error> {
        // The kernel writes the new timer id here; a null pointer is EFAULT.
        let timerid_ptr = call.timerid().ok_or(Errno::EFAULT)?;
        let clockid = call.clockid();
        match clockid {
            libc::CLOCK_REALTIME | libc::CLOCK_MONOTONIC => {}
            libc::CLOCK_PROCESS_CPUTIME_ID | libc::CLOCK_THREAD_CPUTIME_ID => {
                warn!("timer_create clock {clockid} is not virtualized for expiration delivery");
                return Err(Errno::ENOSYS.into());
            }
            _ => return Err(Errno::EINVAL.into()),
        }

        let default_signal = call.sevp().is_none();
        let notification = if let Some(event_ptr) = call.sevp() {
            let event: libc::sigevent = guest.memory().read_value(event_ptr)?;
            match event.sigev_notify {
                libc::SIGEV_NONE => None,
                libc::SIGEV_SIGNAL => {
                    let signal = event.sigev_signo;
                    let standard = Signal::try_from(signal).is_ok();
                    let realtime = (libc::SIGRTMIN()..=libc::SIGRTMAX()).contains(&signal);
                    if !standard && !realtime {
                        return Err(Errno::EINVAL.into());
                    }
                    let value = event.sigev_value.sival_ptr as usize as u64;
                    Some((signal, value))
                }
                _ => return Err(Errno::ENOSYS.into()),
            }
        } else {
            None
        };
        let id = {
            let mut timers = guest.thread_state().posix_timers.lock().unwrap();
            if default_signal {
                timers.create_default_signal()
            } else {
                timers.create(notification)
            }
        };
        guest
            .memory()
            .write_value(timerid_ptr, &(id as libc::c_int))?;
        detlog!(
            "[dtid {}] timer_create(clockid={:?}) => deterministic timer id {} signal {:?}",
            guest.thread_state().dettid,
            clockid,
            id,
            notification,
        );
        Ok(0)
    }

    /// timer_settime: arm or disarm a timer against the deterministic virtual
    /// clock. The old arming is reported through `old_value` when requested.
    pub async fn handle_timer_settime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerSettime,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        if call.flags() & !libc::TIMER_ABSTIME != 0 {
            return Err(Errno::EINVAL.into());
        }
        let new_ptr = call.new_value().ok_or(Errno::EINVAL)?;
        let new: libc::itimerspec = guest.memory().read_value(new_ptr)?;
        let interval_ns = timespec_to_ns(new.it_interval)?;
        let value_ns = timespec_to_ns(new.it_value)?;
        let configured_signal = {
            let timers = guest.thread_state().posix_timers.lock().unwrap();
            timers.signal(id)
        }
        .ok_or(Errno::EINVAL)?;
        if interval_ns != 0 {
            return Err(Errno::ENOSYS.into());
            if value_ns != 0
                && let Some((signal, _)) = configured_signal
                && Signal::try_from(signal).is_err()
            {
                let dettid = guest.thread_state().dettid;
                let status = Process::new(dettid.as_raw())
                    .and_then(|process| process.status())
                    .map_err(|error| {
                        warn!("[dtid {dettid}] cannot inspect realtime timer signal mask: {error}");
                        Errno::ENOSYS
                    })?;
                let signal_bit = 1_u64 << (signal - 1);
                if status.sigblk & signal_bit == 0 && status.sigign & signal_bit == 0 {
                    warn!(
                        "[dtid {dettid}] unblocked realtime POSIX timer signal {signal} is unsupported by the active backend"
                    );
                    return Err(Errno::ENOSYS.into());
                }
            }
        }

        let now = thread_observe_time(guest).await;
        let deadline = if value_ns == 0 {
            None
        } else if call.flags() & libc::TIMER_ABSTIME != 0 {
            // Absolute expiration is interpreted against the same virtual clock.
            Some(LogicalTime::from_nanos(value_ns))
        } else {
            Some(now + Duration::from_nanos(value_ns))
        };

        let old = {
            let mut timers = guest.thread_state().posix_timers.lock().unwrap();
            timers.settime(id, interval_ns, deadline, now)
        };
        let (old_remaining_ns, old_interval_ns) = old.ok_or(Errno::EINVAL)?;

        if let Some(old_ptr) = call.old_value() {
            let old_spec = libc::itimerspec {
                it_interval: ns_to_timespec(old_interval_ns),
                it_value: ns_to_timespec(old_remaining_ns),
            };
            guest.memory().write_value(old_ptr, &old_spec)?;
        }

        let duration = if configured_signal.is_some() {
            deadline.map(|deadline| {
                LogicalTime::from_nanos(deadline.as_nanos().saturating_sub(now.as_nanos()))
            })
        } else {
            None
        };
        update_posix_timer(guest, id, duration, configured_signal).await;

        detlog!(
            "[dtid {}] timer_settime(id={}, interval_ns={}, value_ns={}) updated scheduler deadline {:?}",
            guest.thread_state().dettid,
            id,
            interval_ns,
            value_ns,
            deadline,
        );
        Ok(0)
    }

    /// timer_gettime: report the time remaining until the next expiration and
    /// the reload interval, both computed from the virtual clock.
    pub async fn handle_timer_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerGettime,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        let value_ptr = call.value().ok_or(Errno::EFAULT)?;
        let now = thread_observe_time(guest).await;
        let cur = {
            let timers = guest.thread_state().posix_timers.lock().unwrap();
            timers.gettime(id, now)
        };
        let (remaining_ns, interval_ns) = cur.ok_or(Errno::EINVAL)?;
        let spec = libc::itimerspec {
            it_interval: ns_to_timespec(interval_ns),
            it_value: ns_to_timespec(remaining_ns),
        };
        guest.memory().write_value(value_ptr, &spec)?;
        Ok(0)
    }

    /// timer_getoverrun: one-shot timers cannot overrun, and periodic signal
    /// timers are rejected until deterministic reload accounting is modeled.
    pub async fn handle_timer_getoverrun<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerGetoverrun,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        let exists = guest
            .thread_state()
            .posix_timers
            .lock()
            .unwrap()
            .contains(id);
        if exists {
            Ok(0)
        } else {
            Err(Errno::EINVAL.into())
        }
    }

    /// timer_delete: destroy a timer created by `timer_create`.
    pub async fn handle_timer_delete<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerDelete,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        let existed = {
            let mut timers = guest.thread_state().posix_timers.lock().unwrap();
            timers.remove(id)
        };
        if existed {
            update_posix_timer(guest, id, None, None).await;
            detlog!(
                "[dtid {}] timer_delete(id={})",
                guest.thread_state().dettid,
                id,
            );
            Ok(0)
        } else {
            Err(Errno::EINVAL.into())
        }
    }
}
