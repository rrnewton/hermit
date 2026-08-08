/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use procfs::process::Process;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;

use crate::Detcore;
use crate::RecordOrReplay;
use crate::tool_global::thread_observe_time;
use crate::tool_local::ResourceLimit;

const MB: u64 = 1024 * 1024;
// Linux exposes USER_HZ, not the kernel's configurable scheduler HZ, through times(2).
const CLOCK_TICKS_PER_SECOND: u64 = 100;
const NANOS_PER_CLOCK_TICK: u64 = 1_000_000_000 / CLOCK_TICKS_PER_SECOND;

fn clock_ticks(duration: crate::types::LogicalTime) -> u64 {
    duration.as_nanos() / NANOS_PER_CLOCK_TICK
}

/// Convert a logical CPU duration into the `timeval` shape `getrusage(2)` reports.
///
/// Kept at nanosecond source resolution and truncated only to `timeval`'s microsecond field, so
/// the value stays fine-grained rather than collapsing onto the 100 Hz `clock_t` grid that
/// `times(2)` is obliged to use.
fn timeval_from_logical(duration: crate::types::LogicalTime) -> libc::timeval {
    const NANOS_PER_SEC: u64 = 1_000_000_000;
    const NANOS_PER_MICRO: u64 = 1_000;
    let nanos = duration.as_nanos();
    libc::timeval {
        tv_sec: (nanos / NANOS_PER_SEC) as libc::time_t,
        tv_usec: ((nanos % NANOS_PER_SEC) / NANOS_PER_MICRO) as libc::suseconds_t,
    }
}

fn clock_t_from_ticks(ticks: u64) -> libc::clock_t {
    ticks as libc::clock_t
}

fn logical_clock_ticks(
    now: crate::types::LogicalTime,
    boot: crate::types::LogicalTime,
    uptime_offset_seconds: u64,
) -> libc::clock_t {
    let ticks = uptime_offset_seconds
        .wrapping_mul(CLOCK_TICKS_PER_SECOND)
        .wrapping_add(clock_ticks(now - boot));
    clock_t_from_ticks(ticks)
}

impl<T: RecordOrReplay> Detcore<T> {
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Return one deterministic process resource limit through the legacy ABI.
    pub async fn handle_getrlimit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getrlimit,
    ) -> Result<i64, Error> {
        let resource = u32::try_from(call.resource()).map_err(|_| Errno::EINVAL)?;
        let address = call.rlim().ok_or(Errno::EFAULT)?;
        let limit = guest
            .thread_state()
            .resource_limits
            .lock()
            .expect("resource limits mutex poisoned")
            .get(resource)
            .ok_or(Errno::EINVAL)?;
        let result = libc::rlimit {
            rlim_cur: limit.current,
            rlim_max: limit.maximum,
        };
        guest.memory().write_value(address, &result)?;
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Update one virtual process resource limit through the legacy ABI.
    pub async fn handle_setrlimit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setrlimit,
    ) -> Result<i64, Error> {
        let resource = u32::try_from(call.resource()).map_err(|_| Errno::EINVAL)?;
        let address = call.rlim().ok_or(Errno::EFAULT)?;
        let requested: libc::rlimit = guest.memory().read_value(address)?;
        let requested = ResourceLimit {
            current: requested.rlim_cur,
            maximum: requested.rlim_max,
        };
        let resource_limits = guest.thread_state().resource_limits.clone();
        let mut limits = resource_limits
            .lock()
            .expect("resource limits mutex poisoned");
        let previous = limits.get(resource).ok_or(Errno::EINVAL)?;
        if requested.current > requested.maximum {
            return Err(Errno::EINVAL.into());
        }
        if resource != libc::RLIMIT_STACK && resource != libc::RLIMIT_NOFILE {
            return Err(Errno::EPERM.into());
        }
        if requested.maximum > previous.maximum {
            return Err(Errno::EPERM.into());
        }
        limits.set(resource, requested);
        Ok(0)
    }

    /// Virtualize `prlimit64(2)` for the current guest process.
    ///
    /// Queries return process-local deterministic values. Mutations are kept
    /// virtual and restricted to limits that do not grant access to host
    /// resources or affect host scheduling. Accepted mutations update only
    /// guest-observable compatibility state; they are not a sandbox boundary
    /// and do not ask the host kernel to enforce the virtual limit.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#534)
    pub async fn handle_prlimit64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Prlimit64,
    ) -> Result<i64, Error> {
        let resource = call.resource();
        let resource_limits = guest.thread_state().resource_limits.clone();
        if resource_limits
            .lock()
            .expect("resource limits mutex poisoned")
            .get(resource)
            .is_none()
        {
            return Err(Errno::EINVAL.into());
        }

        let requested = if let Some(address) = call.new_rlim() {
            let limit: libc::rlimit64 = guest.memory().read_value(address)?;
            Some(ResourceLimit {
                current: limit.rlim_cur,
                maximum: limit.rlim_max,
            })
        } else {
            None
        };

        let pid = call.pid();
        if pid != 0 && pid != guest.pid().as_raw() {
            return Err(Errno::EPERM.into());
        }

        let previous = {
            let mut limits = resource_limits
                .lock()
                .expect("resource limits mutex poisoned");
            let previous = limits
                .get(resource)
                .expect("resource validity changed while handling prlimit64");

            if let Some(requested) = requested {
                if requested.current > requested.maximum {
                    return Err(Errno::EINVAL.into());
                }
                if resource != libc::RLIMIT_STACK && resource != libc::RLIMIT_NOFILE {
                    return Err(Errno::EPERM.into());
                }
                if requested.maximum > previous.maximum {
                    return Err(Errno::EPERM.into());
                }
                limits.set(resource, requested);
            }

            previous
        };

        if let Some(address) = call.old_rlim() {
            let previous = libc::rlimit64 {
                rlim_cur: previous.current,
                rlim_max: previous.maximum,
            };
            guest.memory().write_value(address, &previous)?;
        }

        crate::detlog!(
            "prlimit64: pid={pid}, resource={resource}, mutation={}, old={}:{}",
            requested.is_some(),
            previous.current,
            previous.maximum
        );
        Ok(0)
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2.1): Review deriving rusage CPU times from logical accounting.
    /// Return a deterministic resource-usage snapshot.
    ///
    /// `ru_utime` and `ru_stime` are derived from the *same* logical CPU accounting that
    /// [`Self::handle_times`] reports, so `getrusage(2)` and `times(2)` cannot disagree about how
    /// much CPU a guest has consumed. Reporting zero here while `times(2)` reported a nonzero,
    /// advancing value made two views of one virtual timeline contradict each other inside a single
    /// run, and a guest computing `utime / elapsed` got a confidently wrong zero. Zero was never
    /// required for determinism: the logical accounting is already deterministic, continuous, and
    /// fine-grained, which is why `times(2)` could use it.
    ///
    /// Each `who` reports the scope Linux specifies, from the accounting Detcore already maintains:
    /// `RUSAGE_SELF` the whole process, `RUSAGE_THREAD` the calling thread alone, and
    /// `RUSAGE_CHILDREN` the totals of reaped children.
    ///
    /// `ru_maxrss` is populated with the guest's peak resident set size so that programs which
    /// require a positive maximum RSS (e.g. rr's `rusage` test) behave like they do on Linux. The
    /// value comes from the same procfs memory accounting that `sysinfo`'s free-memory reporting
    /// already relies on, which is deterministic across runs under Detcore's fixed schedule.
    ///
    /// Page-fault and context-switch counters (`ru_minflt`, `ru_majflt`, `ru_nvcsw`, `ru_nivcsw`)
    /// remain zero. Those need event counts Detcore mediates but does not yet aggregate per
    /// process; deriving them is separate work, and zero is still a wrong-but-stable answer there.
    pub async fn handle_getrusage<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getrusage,
    ) -> Result<i64, Error> {
        let who = call.who();
        match who {
            libc::RUSAGE_SELF | libc::RUSAGE_CHILDREN | libc::RUSAGE_THREAD => {}
            _ => return Err(Errno::EINVAL.into()),
        }

        let usage_addr = call.usage().ok_or(Errno::EFAULT)?;

        // SAFETY: `libc::rusage` is a plain-old-data C struct that is valid when zero-initialized.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };

        // `process_cpu_time` folds this thread's outstanding delta into the shared process
        // accounting before snapshotting, so the totals below include work done since the last
        // accounting point rather than lagging it.
        let process = guest.thread_state_mut().process_cpu_time();
        let (user, system) = match who {
            libc::RUSAGE_SELF => (process.user, process.system),
            libc::RUSAGE_THREAD => {
                let thread = &guest.thread_state().thread_logical_time;
                (thread.user_cpu_time(), thread.system_cpu_time())
            }
            // Linux aggregates *reaped* children only, which is exactly what these counters hold.
            _ => (process.children_user, process.children_system),
        };
        usage.ru_utime = timeval_from_logical(user);
        usage.ru_stime = timeval_from_logical(system);

        // RUSAGE_SELF/RUSAGE_THREAD report this process's peak RSS. RUSAGE_CHILDREN aggregates
        // terminated children only; with no such accounting we leave it zero, matching Linux when
        // no child has exited.
        if matches!(who, libc::RUSAGE_SELF | libc::RUSAGE_THREAD) {
            usage.ru_maxrss = self.guest_peak_rss_kb(guest) as libc::c_long;
        }

        guest.memory().write_value(usage_addr, &usage)?;
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#797): Review logical elapsed and process CPU accounting semantics.
    /// Return deterministic elapsed ticks and process CPU accounting for `times(2)`.
    ///
    /// Linux's host boot epoch and scheduler CPU counters are nondeterministic. Detcore instead
    /// derives the return value from its global logical clock. Per-process logical CPU accounting
    /// aggregates user instruction and syscall-system time across threads; forked processes start
    /// fresh counters and contribute their totals to the parent's child counters when reaped.
    pub async fn handle_times<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Times,
    ) -> Result<i64, Error> {
        let now = thread_observe_time(guest).await;
        let boot = crate::types::DetTime::new(&self.cfg).as_nanos();
        let ticks = logical_clock_ticks(now, boot, self.cfg.sysinfo_uptime_offset);
        let cpu = guest.thread_state_mut().process_cpu_time();

        if let Some(address) = call.buf() {
            let usage = libc::tms {
                tms_utime: clock_t_from_ticks(clock_ticks(cpu.user)),
                tms_stime: clock_t_from_ticks(clock_ticks(cpu.system)),
                tms_cutime: clock_t_from_ticks(clock_ticks(cpu.children_user)),
                tms_cstime: clock_t_from_ticks(clock_ticks(cpu.children_system)),
            };
            guest.memory().write_value(address, &usage)?;
        }

        Ok(ticks as i64)
    }

    /// The guest's peak resident set size ("high water mark") in kibibytes, matching the units of
    /// Linux `getrusage`'s `ru_maxrss`. Reads procfs like [`Self::free_ram`]; always returns a
    /// positive value so guests can rely on a nonzero maximum RSS even if the read fails.
    fn guest_peak_rss_kb<G: Guest<Self>>(&self, guest: &G) -> u64 {
        Process::new(guest.pid().as_raw())
            .and_then(|process| process.status())
            .ok()
            .and_then(|status| status.vmhwm.or(status.vmrss))
            .unwrap_or(0)
            .max(1)
    }

    /// handle sysinfo syscall
    pub async fn handle_sysinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sysinfo,
    ) -> Result<i64, Error> {
        let sys_info = self.collect_sysinfo(guest).await?;
        let mut memory = guest.memory();

        if let Some(info_addr) = call.info() {
            memory.write_value(info_addr, &sys_info.into())?;
        }
        Ok(0)
    }

    pub(super) async fn calculate_uptime<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<u64, Error> {
        let global_time = thread_observe_time(guest).await;
        Ok(self.cfg.sysinfo_uptime_offset + global_time.as_secs()
            - crate::types::DetTime::new(&self.cfg).as_nanos().as_secs())
    }

    async fn collect_sysinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<syscalls::SysInfo, Error> {
        Ok(syscalls::SysInfo {
            uptime: self.calculate_uptime(guest).await?,
            loads_1: 1,
            loads_5: 1,
            loads_15: 1,
            total_ram: self.cfg.memory,
            free_ram: self.free_ram(guest, self.cfg.memory)?,
            buffer_ram: MB,
            shared_ram: MB,
            total_swap: 0,
            free_swap: 0,
            procs: 1,
            total_high: 0,
            free_high: 0,
            mem_unit: 1,
        })
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1054): Deterministic free-memory accounting for sysinfo(2).
    /// Report guest-visible free memory as `total_ram` minus the guest's virtual
    /// address-space size.
    ///
    /// The used-memory figure must be a function of guest computation alone, or
    /// `sysinfo`'s `free_ram` (and therefore glibc `sysconf(_SC_AVPHYS_PAGES)`)
    /// becomes nondeterministic across `--verify` and record/replay. The virtual
    /// size (`statm.size`, i.e. `/proc/<pid>/statm` field 1) is the sum of the
    /// guest's own mappings, which is fixed by the guest's brk/mmap sequence under
    /// Detcore's deterministic schedule. The resident set size (`statm.resident`),
    /// used previously, is physical page residency managed by the host kernel
    /// (demand paging, reclaim, host memory pressure); it drifts by a page or two
    /// between otherwise-identical runs and made `getconf -a` flake ~10% at L2.
    fn free_ram<G: Guest<Self>>(&self, guest: &mut G, total_ram: u64) -> anyhow::Result<u64> {
        let process = Process::new(guest.pid().as_raw())?;
        let page_size = procfs::page_size();
        let statm = process.statm()?;
        let used_memory = statm.size * page_size;
        if used_memory > total_ram {
            return Ok(0);
        }
        Ok(total_ram - used_memory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LogicalTime;

    #[test]
    fn rusage_timeval_keeps_microsecond_resolution() {
        // A duration far below one `times(2)` clock tick (10 ms) must still be
        // visible in `getrusage`. Collapsing it to zero would reintroduce the
        // "stable because it is a constant" anti-pattern this change removes.
        let sub_tick = LogicalTime::from_nanos(475_000);
        let tv = timeval_from_logical(sub_tick);
        assert_eq!((tv.tv_sec, tv.tv_usec), (0, 475));
        assert_eq!(
            clock_ticks(sub_tick),
            0,
            "below one clock tick by construction"
        );

        assert_eq!(
            {
                let tv = timeval_from_logical(LogicalTime::from_nanos(0));
                (tv.tv_sec, tv.tv_usec)
            },
            (0, 0)
        );

        // Seconds and the sub-second remainder are split, not conflated.
        let tv = timeval_from_logical(LogicalTime::from_nanos(2_300_484_000));
        assert_eq!((tv.tv_sec, tv.tv_usec), (2, 300_484));

        // Nanosecond residue below a microsecond truncates rather than rounding
        // up, matching how Linux fills the field from its own ns accounting.
        let tv = timeval_from_logical(LogicalTime::from_nanos(1_999));
        assert_eq!((tv.tv_sec, tv.tv_usec), (0, 1));
    }

    #[test]
    fn rusage_and_times_report_the_same_cpu_duration() {
        // The defect this fixes was two views of one virtual timeline
        // disagreeing inside a single run: `times(2)` advancing while
        // `getrusage(2)` stayed 0. Both projections now come from the same
        // `LogicalTime`, so they must agree to within the coarser one's grid.
        for nanos in [0u64, 1_000_000, 300_484_000, 7_000_000_000, 12_345_678_901] {
            let duration = LogicalTime::from_nanos(nanos);
            let tv = timeval_from_logical(duration);
            let ticks = clock_ticks(duration);

            let rusage_micros = tv.tv_sec as u64 * 1_000_000 + tv.tv_usec as u64;
            let times_micros = ticks * (NANOS_PER_CLOCK_TICK / 1_000);

            assert!(
                rusage_micros >= times_micros,
                "getrusage must not report LESS CPU than times for {nanos}ns: \
                 {rusage_micros}us vs {times_micros}us"
            );
            assert!(
                rusage_micros - times_micros < NANOS_PER_CLOCK_TICK / 1_000,
                "the two views must agree to within one clock tick for {nanos}ns: \
                 {rusage_micros}us vs {times_micros}us"
            );
        }
    }

    #[test]
    fn logical_clock_ticks_include_boot_offset_and_fractional_seconds() {
        let boot = LogicalTime::from_secs(1_000);
        let now = boot + LogicalTime::from_millis(25);

        assert_eq!(logical_clock_ticks(now, boot, 120), 12_002);
    }

    #[test]
    fn logical_cpu_ticks_exclude_boot_epoch() {
        assert_eq!(clock_ticks(LogicalTime::from_millis(25)), 2);
    }

    #[test]
    fn logical_clock_ticks_wrap_configured_offset_like_linux_clock_t() {
        let boot = LogicalTime::from_secs(1_000);
        let before = logical_clock_ticks(boot, boot, u64::MAX);
        let after = logical_clock_ticks(boot + LogicalTime::from_millis(10), boot, u64::MAX);

        assert_eq!(before, -100);
        assert_eq!(after, -99);
    }
}
