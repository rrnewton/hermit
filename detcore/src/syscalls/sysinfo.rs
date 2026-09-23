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

// Linux exposes USER_HZ, not the kernel's configurable scheduler HZ, through times(2).
const CLOCK_TICKS_PER_SECOND: u64 = 100;
const NANOS_PER_CLOCK_TICK: u64 = 1_000_000_000 / CLOCK_TICKS_PER_SECOND;

fn clock_ticks(duration: crate::types::LogicalTime) -> u64 {
    duration.as_nanos() / NANOS_PER_CLOCK_TICK
}

fn clock_t_from_ticks(ticks: u64) -> libc::clock_t {
    ticks as libc::clock_t
}

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const NANOS_PER_MICROSECOND: u64 = 1_000;

/// Render a logical CPU duration as the `timeval` `getrusage(2)` reports.
///
/// Linux truncates rusage CPU times to microsecond granularity, so the sub-microsecond
/// remainder of the logical duration is discarded rather than rounded. Truncation (not
/// rounding) is what keeps the value monotonic: a duration that grows by less than a
/// microsecond must never make the reported total go backwards, and rounding-to-nearest
/// on a shrinking remainder can do exactly that.
fn timeval_from_logical(duration: crate::types::LogicalTime) -> libc::timeval {
    let nanos = duration.as_nanos();
    libc::timeval {
        tv_sec: (nanos / NANOS_PER_SECOND) as libc::time_t,
        tv_usec: ((nanos % NANOS_PER_SECOND) / NANOS_PER_MICROSECOND) as libc::suseconds_t,
    }
}

fn logical_clock_ticks(
    now: crate::types::LogicalTime,
    boot: crate::types::LogicalTime,
    uptime_offset_seconds: u64,
) -> anyhow::Result<libc::clock_t> {
    // The programmatic uptime offset can exceed every Linux ABI domain. Keep
    // one explicit extreme-input policy across the three projections below:
    // preserve their native ABI behavior (clock_t wraps, signed uptime clamps
    // high, unsigned btime clamps low) rather than letting Rust arithmetic
    // panic. These boundary values need not preserve btime + uptime == now;
    // normal in-domain offsets retain that Linux relationship exactly.
    let elapsed_nanos = now
        .as_nanos()
        .checked_sub(boot.as_nanos())
        .ok_or_else(|| anyhow::anyhow!("logical clock regressed before its boot origin"))?;
    let ticks = uptime_offset_seconds
        .wrapping_mul(CLOCK_TICKS_PER_SECOND)
        .wrapping_add(clock_ticks(crate::types::LogicalTime::from_nanos(
            elapsed_nanos,
        )));
    Ok(clock_t_from_ticks(ticks))
}

fn logical_uptime_seconds(
    now: crate::types::LogicalTime,
    boot: crate::types::LogicalTime,
    uptime_offset_seconds: u64,
) -> anyhow::Result<u64> {
    // Subtract in the full nanosecond domain before projecting the elapsed
    // duration to whole seconds. Flooring `now` and `boot` independently makes
    // uptime jump a second early whenever the absolute timestamps straddle a
    // second boundary but less than one full second has elapsed.
    // Linux exposes uptime through a signed `c_long`. An extreme programmatic
    // offset must not overflow in this unsigned intermediate and then become a
    // negative guest-visible uptime when the syscall ABI is populated. As with
    // `logical_boot_time_seconds`, clamp only at the ABI boundary.
    let elapsed_nanos = now
        .as_nanos()
        .checked_sub(boot.as_nanos())
        .ok_or_else(|| anyhow::anyhow!("logical uptime regressed before its boot origin"))?;
    Ok(uptime_offset_seconds
        .saturating_add(crate::types::LogicalTime::from_nanos(elapsed_nanos).as_secs())
        .min(libc::c_long::MAX as u64))
}

pub(super) fn logical_boot_time_seconds(
    boot: crate::types::LogicalTime,
    uptime_offset_seconds: u64,
) -> u64 {
    // Linux renders /proc/stat's btime with `%llu`. Hermit's epoch domain is
    // nonnegative, so an uptime offset that predates that domain is represented
    // by the earliest faithful value, zero, rather than a Linux-impossible
    // negative token. Saturation also keeps an unrelated procfs snapshot
    // readable for extreme programmatic offsets.
    boot.as_secs().saturating_sub(uptime_offset_seconds)
}

fn prlimit_targets_current_process(
    target_pid: i32,
    deterministic_pid: Option<i32>,
    physical_pid: i32,
) -> bool {
    target_pid == 0 || target_pid == deterministic_pid.unwrap_or(physical_pid)
}

fn validate_resource_limit_mutation(
    resource: u32,
    previous: ResourceLimit,
    requested: ResourceLimit,
) -> Result<(), Errno> {
    if requested.current > requested.maximum {
        return Err(Errno::EINVAL);
    }
    // Linux accepts an exact no-op for every valid resource, including limits
    // that an unprivileged process could not otherwise change. Recognize that
    // case before applying Detcore's narrower virtual-mutation policy.
    if requested == previous {
        return Ok(());
    }
    // CORE is virtual compatibility state too: changing it cannot enable host
    // core dumps, while Linux sanitizers routinely lower its soft limit.
    if resource != libc::RLIMIT_STACK
        && resource != libc::RLIMIT_NOFILE
        && resource != libc::RLIMIT_CORE
    {
        return Err(Errno::EPERM);
    }
    if requested.maximum > previous.maximum {
        return Err(Errno::EPERM);
    }
    Ok(())
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
        validate_resource_limit_mutation(resource, previous, requested)?;
        if requested != previous {
            limits.set(resource, requested);
        }
        Ok(0)
    }

    /// Virtualize `prlimit64(2)` for the current guest process.
    ///
    /// Queries return process-local deterministic values. Exact no-op updates
    /// succeed for every valid resource, as on Linux. Changes are kept virtual
    /// and restricted to limits that do not grant access to host resources or
    /// affect host scheduling. Accepted changes update only guest-observable
    /// compatibility state; they are not a sandbox boundary and do not ask the
    /// host kernel to enforce the virtual limit.
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
        let deterministic_pid = guest.thread_state().detpid.map(|detpid| detpid.as_raw());
        if !prlimit_targets_current_process(pid, deterministic_pid, guest.pid().as_raw()) {
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
                validate_resource_limit_mutation(resource, previous, requested)?;
                if requested != previous {
                    limits.set(resource, requested);
                }
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
    /// Return a deterministic resource-usage snapshot.
    ///
    /// `ru_utime`/`ru_stime` come from the SAME logical CPU accounting that backs `times(2)`
    /// (see [`Self::handle_times`]), not from host scheduler counters. Reporting them as zero,
    /// as this did previously, was both a fidelity bug and an internal contradiction: a guest
    /// that called `times(2)` saw advancing CPU time while `getrusage(2)` insisted the same
    /// process had consumed none. Deriving both from `ProcessCpuSnapshot` makes the two
    /// syscalls agree by construction rather than by coincidence.
    ///
    /// The `who` values report different aggregates, matching Linux:
    /// - `RUSAGE_SELF` — this process, summed across its threads.
    /// - `RUSAGE_THREAD` — the calling thread alone. This reads the thread's own logical CPU
    ///   counters rather than the process totals; substituting the process aggregate would
    ///   over-report for every multithreaded guest.
    /// - `RUSAGE_CHILDREN` — reaped children only, which is exactly what the `children_*`
    ///   fields accumulate on `wait`.
    ///
    /// `ru_maxrss` is populated for the process/thread cases with the guest's peak resident set
    /// size so that programs which require a positive maximum RSS (e.g. rr's `rusage` test)
    /// behave like they do on Linux. This remains a best-effort host-procfs observation on
    /// backends where [`Guest::pid`] names a host process; it is separate from the configured
    /// system-wide memory reported by `sysinfo(2)` and virtual `/proc/meminfo`.
    ///
    /// Page-fault and context-switch counts remain zero: Detcore does not model them, and
    /// synthesizing a plausible-looking number would be worse than reporting none.
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

        let (user, system) = match who {
            libc::RUSAGE_THREAD => guest.thread_state_mut().thread_cpu_time(),
            libc::RUSAGE_CHILDREN => {
                let cpu = guest.thread_state_mut().process_cpu_time();
                (cpu.children_user, cpu.children_system)
            }
            // RUSAGE_SELF
            _ => {
                let cpu = guest.thread_state_mut().process_cpu_time();
                (cpu.user, cpu.system)
            }
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
        let ticks = logical_clock_ticks(now, boot, self.cfg.sysinfo_uptime_offset)?;
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
    /// Linux `getrusage`'s `ru_maxrss`. This reads host procfs through [`Guest::pid`], which only
    /// identifies the guest process on backends where it names a host process; always returns a
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
        let info_addr = call.info().ok_or(Errno::EFAULT)?;
        let sys_info = self.collect_sysinfo(guest).await?;
        let mut memory = guest.memory();

        memory.write_value(info_addr, &sys_info.into())?;
        Ok(0)
    }

    pub(super) async fn calculate_uptime<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<u64, Error> {
        let global_time = thread_observe_time(guest).await;
        Ok(logical_uptime_seconds(
            global_time,
            crate::types::DetTime::new(&self.cfg).as_nanos(),
            self.cfg.sysinfo_uptime_offset,
        )?)
    }

    async fn collect_sysinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<syscalls::SysInfo, Error> {
        let memory = configured_memory(self.cfg.memory);
        Ok(syscalls::SysInfo {
            uptime: self.calculate_uptime(guest).await?,
            loads_1: 1,
            loads_5: 1,
            loads_15: 1,
            total_ram: memory.total_ram,
            free_ram: memory.free_ram,
            buffer_ram: memory.buffer_ram,
            shared_ram: memory.shared_ram,
            total_swap: memory.total_swap,
            free_swap: memory.free_swap,
            procs: 1,
            total_high: memory.total_high,
            free_high: memory.free_high,
            mem_unit: memory.mem_unit,
        })
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-2979): Deterministic free-memory accounting for sysinfo(2).
#[derive(Debug, PartialEq, Eq)]
struct ConfiguredMemory {
    total_ram: u64,
    free_ram: u64,
    buffer_ram: u64,
    shared_ram: u64,
    total_swap: u64,
    free_swap: u64,
    total_high: u64,
    free_high: u64,
    mem_unit: u32,
}

/// Report the configured guest memory consistently with virtual `/proc/meminfo`.
///
/// Linux `sysinfo(2)` describes system-wide memory, not one process's virtual
/// mappings. Detcore does not model allocation pressure within its configured
/// memory limit, so all configured memory remains available and the other
/// modeled memory categories remain empty.
fn configured_memory(memory: u64) -> ConfiguredMemory {
    ConfiguredMemory {
        total_ram: memory,
        free_ram: memory,
        buffer_ram: 0,
        shared_ram: 0,
        total_swap: 0,
        free_swap: 0,
        total_high: 0,
        free_high: 0,
        mem_unit: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LogicalTime;

    #[test]
    fn logical_clock_ticks_include_boot_offset_and_fractional_seconds() {
        let boot = LogicalTime::from_secs(1_000);
        let now = boot + LogicalTime::from_millis(25);

        assert_eq!(logical_clock_ticks(now, boot, 120).unwrap(), 12_002);
    }

    #[test]
    fn logical_uptime_floors_elapsed_time_not_absolute_endpoints() {
        let boot = LogicalTime::from_nanos(1_790_000_000_970_859_833);

        // This crosses an absolute whole-second boundary after only 29ms. The
        // old floor(now)-floor(boot) expression incorrectly reported +1s.
        let crossed_boundary = boot + LogicalTime::from_nanos(29_140_167);
        assert_eq!(
            logical_uptime_seconds(crossed_boundary, boot, 120).unwrap(),
            120
        );
        assert_eq!(logical_uptime_seconds(boot, boot, 120).unwrap(), 120);
        assert_eq!(logical_boot_time_seconds(boot, 120), 1_789_999_880);
        assert_eq!(logical_boot_time_seconds(LogicalTime::ZERO, 120), 0);
        assert_eq!(logical_boot_time_seconds(LogicalTime::ZERO, u64::MAX), 0,);
    }

    #[test]
    fn logical_uptime_advances_after_one_exact_elapsed_second() {
        let boot = LogicalTime::from_nanos(1_790_000_000_970_859_833);
        assert_eq!(
            logical_uptime_seconds(boot + LogicalTime::from_secs(1), boot, 120).unwrap(),
            121,
        );
    }

    #[test]
    fn logical_uptime_saturates_at_linux_long_boundary() {
        let boot = LogicalTime::from_nanos(1_790_000_000_970_859_833);
        let max_linux_uptime = libc::c_long::MAX as u64;

        assert_eq!(
            logical_uptime_seconds(boot + LogicalTime::from_secs(2), boot, max_linux_uptime - 1,)
                .unwrap(),
            max_linux_uptime,
        );
        assert_eq!(
            logical_uptime_seconds(boot + LogicalTime::from_secs(1), boot, u64::MAX).unwrap(),
            max_linux_uptime,
        );

        let regressed = boot - LogicalTime::from_nanos(1);
        assert!(logical_uptime_seconds(regressed, boot, 120).is_err());
        assert!(logical_clock_ticks(regressed, boot, 120).is_err());
    }

    #[test]
    fn sysinfo_memory_matches_configured_memory() {
        assert_eq!(
            configured_memory(1_000_000_000),
            ConfiguredMemory {
                total_ram: 1_000_000_000,
                free_ram: 1_000_000_000,
                buffer_ram: 0,
                shared_ram: 0,
                total_swap: 0,
                free_swap: 0,
                total_high: 0,
                free_high: 0,
                mem_unit: 1,
            },
        );
    }

    #[test]
    fn prlimit_self_target_prefers_deterministic_process_identity() {
        assert!(prlimit_targets_current_process(3, Some(3), 10_003));
        assert!(prlimit_targets_current_process(0, Some(3), 10_003));
        assert!(!prlimit_targets_current_process(10_003, Some(3), 10_003));
        assert!(!prlimit_targets_current_process(4, Some(3), 10_003));
    }

    #[test]
    fn prlimit_self_target_falls_back_to_physical_identity_before_init() {
        assert!(prlimit_targets_current_process(10_003, None, 10_003));
        assert!(!prlimit_targets_current_process(3, None, 10_003));
    }

    #[test]
    fn prlimit_accepts_exact_noop_for_restricted_resource() {
        let limit = ResourceLimit {
            current: 0,
            maximum: 0,
        };
        assert_eq!(
            validate_resource_limit_mutation(libc::RLIMIT_CPU, limit, limit),
            Ok(())
        );
    }

    #[test]
    fn prlimit_accepts_core_soft_limit_change() {
        let previous = ResourceLimit {
            current: 1,
            maximum: 1,
        };
        let requested = ResourceLimit {
            current: 0,
            maximum: 1,
        };
        assert_eq!(
            validate_resource_limit_mutation(libc::RLIMIT_CORE, previous, requested),
            Ok(())
        );
    }

    #[test]
    fn prlimit_rejects_actual_change_to_restricted_resource() {
        let previous = ResourceLimit {
            current: 1,
            maximum: 1,
        };
        let requested = ResourceLimit {
            current: 0,
            maximum: 1,
        };
        assert_eq!(
            validate_resource_limit_mutation(libc::RLIMIT_CPU, previous, requested),
            Err(Errno::EPERM)
        );
    }

    #[test]
    fn prlimit_rejects_invalid_soft_limit_before_noop_policy() {
        let previous = ResourceLimit {
            current: 1,
            maximum: 1,
        };
        let requested = ResourceLimit {
            current: 2,
            maximum: 1,
        };
        assert_eq!(
            validate_resource_limit_mutation(libc::RLIMIT_CORE, previous, requested),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn prlimit_rejects_core_hard_limit_raise() {
        let previous = ResourceLimit {
            current: 1,
            maximum: 1,
        };
        let requested = ResourceLimit {
            current: 1,
            maximum: 2,
        };
        assert_eq!(
            validate_resource_limit_mutation(libc::RLIMIT_CORE, previous, requested),
            Err(Errno::EPERM)
        );
    }

    #[test]
    fn logical_cpu_ticks_exclude_boot_epoch() {
        assert_eq!(clock_ticks(LogicalTime::from_millis(25)), 2);
    }

    #[test]
    fn rusage_timeval_splits_seconds_and_microseconds() {
        let tv = timeval_from_logical(LogicalTime::from_millis(2_500));
        assert_eq!(tv.tv_sec, 2);
        assert_eq!(tv.tv_usec, 500_000);
    }

    #[test]
    fn rusage_timeval_truncates_sub_microsecond_rather_than_rounding() {
        // 1_999 ns is a hair under 2us. Truncating yields 1us; rounding to nearest would
        // yield 2us and could make a later, larger duration report a SMALLER value once its
        // remainder shrank -- i.e. CPU time going backwards. Pin truncation explicitly.
        let tv = timeval_from_logical(LogicalTime::from_nanos(1_999));
        assert_eq!(tv.tv_sec, 0);
        assert_eq!(tv.tv_usec, 1);
    }

    #[test]
    fn rusage_timeval_is_monotonic_in_the_logical_duration() {
        // The property that matters to a guest: CPU time never goes backwards. Walk a range
        // of nanosecond durations across microsecond and second boundaries and assert the
        // rendered timeval is non-decreasing at every step.
        let mut previous = (0_i64, 0_i64);
        for nanos in (0..3_000_000u64).step_by(997) {
            let tv = timeval_from_logical(LogicalTime::from_nanos(nanos));
            let current = (tv.tv_sec, tv.tv_usec);
            assert!(
                current >= previous,
                "rusage timeval went backwards at {nanos}ns: {previous:?} -> {current:?}"
            );
            previous = current;
        }
    }

    #[test]
    fn rusage_zero_cpu_time_renders_as_zero() {
        let tv = timeval_from_logical(LogicalTime::ZERO);
        assert_eq!(tv.tv_sec, 0);
        assert_eq!(tv.tv_usec, 0);
    }

    #[test]
    fn rusage_and_times_agree_within_one_clock_tick() {
        // Both syscalls project the same logical duration, but times(2) is
        // quantized to USER_HZ while getrusage(2) retains microseconds.
        for nanos in [0u64, 1_000_000, 300_484_000, 7_000_000_000, 12_345_678_901] {
            let duration = LogicalTime::from_nanos(nanos);
            let tv = timeval_from_logical(duration);
            let rusage_micros = tv.tv_sec as u64 * 1_000_000 + tv.tv_usec as u64;
            let times_micros = clock_ticks(duration) * (NANOS_PER_CLOCK_TICK / 1_000);

            assert!(rusage_micros >= times_micros);
            assert!(rusage_micros - times_micros < NANOS_PER_CLOCK_TICK / 1_000);

            // A tick-ALIGNED duration must agree EXACTLY, not merely to within
            // one tick. The bounds above are satisfied at every sample by an
            // implementation carrying a constant sub-tick offset, so without
            // this the suite cannot distinguish that from a correct one.
            if nanos % NANOS_PER_CLOCK_TICK == 0 {
                assert_eq!(rusage_micros, times_micros);
            }
        }
    }

    #[test]
    fn logical_clock_ticks_wrap_configured_offset_like_linux_clock_t() {
        let boot = LogicalTime::from_secs(1_000);
        let before = logical_clock_ticks(boot, boot, u64::MAX).unwrap();
        let after =
            logical_clock_ticks(boot + LogicalTime::from_millis(10), boot, u64::MAX).unwrap();

        assert_eq!(before, -100);
        assert_eq!(after, -99);
    }
}
