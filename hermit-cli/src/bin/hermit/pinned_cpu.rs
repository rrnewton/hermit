/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The CPU a sequentialized run is pinned to.
//!
//! A run with a `max_timeslice` pins its tracer, its notifier threads and
//! every guest thread to one CPU for the whole run, because its retired-branch
//! counts must not see a migration (`RunOpts::pin_threads`). Every ptrace stop
//! then costs several wakeups on that one CPU: the guest stops, its notifier
//! thread wakes, the tracer wakes, and the tracer resumes the guest.
//!
//! So the CPU matters, and a CPU-bound task already on it is the worst case.
//! Linux shares a CPU between cgroups by weight, and it scales a cgroup's
//! weight on one CPU by the share of the cgroup's load that is on that CPU
//! (`calc_group_shares`, kernel/sched/fair.c). A run inside a cgroup that is
//! busy on many other CPUs, as a test runner's is, therefore has almost no
//! weight on its own CPU. Beside a CPU-bound task from a cgroup whose load is
//! concentrated there, each of the run's wakeups waits for that task, and the
//! run does the same work hundreds of times slower
//! (https://github.com/rrnewton/hermit/issues/4824). Measured on a 316-CPU
//! host: one strict verify of an epoll guest took 1.5 s on an idle CPU and
//! 15.5 s on a CPU running a busy loop from another cgroup, while the run's
//! own cgroup kept 24 busy loops on other CPUs. Under a loaded test runner,
//! runs were 170 to 1,400 times slower and hit test deadlines.
//!
//! A uniformly random CPU lands on such a task as often as there are busy
//! CPUs. Instead, a run samples the allowed CPUs' busy time from `/proc/stat`
//! over [`SAMPLE_WINDOW`] and draws uniformly from the ones that were idle, or,
//! when none was, from the least busy. Concurrent runs that sample at the same
//! moment still spread over every idle CPU rather than meeting on one.
//!
//! The choice is host placement only. The guest sees its own virtual CPU, and
//! the run stays on the one CPU it starts on, as before.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::thread;
use std::time::Duration;

/// How long the allowed CPUs' busy time is sampled before a run takes one.
/// `/proc/stat` counts in 1/USER_HZ ticks, 10 ms on Linux, so this is about
/// five ticks per CPU.
pub(crate) const SAMPLE_WINDOW: Duration = Duration::from_millis(50);

/// A CPU is idle when at most this fraction of the sample was busy, expressed
/// as a denominator: one tick in five. Each read rounds to whole ticks, so an
/// idle CPU can show one busy tick, while a CPU-bound task shows four or five.
const IDLE_DENOMINATOR: u64 = 5;

/// One CPU's cumulative time from `/proc/stat`, in USER_HZ ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CpuTime {
    /// idle plus iowait.
    idle: u64,
    /// user, nice, system, idle, iowait, irq, softirq and steal. The guest
    /// columns are already counted in user and nice, so they are not added.
    total: u64,
}

/// Why a run got the CPU it got, for the log line that names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Basis {
    /// The allowed set has one CPU.
    OnlyAllowed,
    /// Drawn from the `idle` CPUs that were idle across the sample, out of the
    /// `sampled` allowed CPUs it covered.
    Idle { idle: usize, sampled: usize },
    /// No sampled CPU was idle: drawn from the `tied` least busy, out of
    /// `sampled`.
    LeastBusy { tied: usize, sampled: usize },
    /// `/proc/stat` was unreadable or covered none of the allowed CPUs: drawn
    /// from every allowed CPU.
    Unsampled,
}

impl fmt::Display for Basis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Basis::OnlyAllowed => write!(f, "the only allowed CPU"),
            Basis::Idle { idle, sampled } => write!(
                f,
                "one of {idle} of {sampled} allowed CPUs idle over {} ms",
                SAMPLE_WINDOW.as_millis()
            ),
            Basis::LeastBusy { tied, sampled } => write!(
                f,
                "none of {sampled} allowed CPUs idle over {} ms; one of the {tied} least busy",
                SAMPLE_WINDOW.as_millis()
            ),
            Basis::Unsampled => {
                write!(f, "drawn from every allowed CPU; /proc/stat gave no sample")
            }
        }
    }
}

/// Per-CPU times from one reading of `/proc/stat`.
pub(crate) type Reading = BTreeMap<usize, CpuTime>;

/// Choose the CPU a run is pinned to from `allowed`, the tracer's affinity
/// mask, and say why. `None` only when `allowed` is empty.
pub(crate) fn choose(allowed: &[usize]) -> Option<(usize, Basis)> {
    choose_with(allowed, sample)
}

/// [`choose`] with the two readings taken by `sample`, which tests replace.
pub(crate) fn choose_with(
    allowed: &[usize],
    sample: impl FnOnce(&[usize]) -> Option<(Reading, Reading)>,
) -> Option<(usize, Basis)> {
    match allowed {
        [] => None,
        [only] => Some((*only, Basis::OnlyAllowed)),
        _ => {
            let (candidates, basis) = sample(allowed)
                .and_then(|(before, after)| candidates(allowed, &before, &after))
                .unwrap_or_else(|| (allowed.to_vec(), Basis::Unsampled));
            Some((candidates[rand::random_range(0..candidates.len())], basis))
        }
    }
}

/// Two readings of `/proc/stat`, [`SAMPLE_WINDOW`] apart.
fn sample(allowed: &[usize]) -> Option<(Reading, Reading)> {
    let read = || {
        fs::read_to_string("/proc/stat")
            .map_err(|error| tracing::debug!("cannot sample CPU load from /proc/stat: {error}"))
            .ok()
            .map(|text| parse_proc_stat(&text, allowed))
    };
    let before = read()?;
    thread::sleep(SAMPLE_WINDOW);
    Some((before, read()?))
}

/// The per-CPU lines of `/proc/stat` for the CPUs in `allowed`:
/// `cpuN user nice system idle iowait irq softirq steal guest guest_nice`,
/// where kernels before 2.6.11 stop earlier. The first line, `cpu`, sums
/// every CPU and has no number, so it is skipped.
fn parse_proc_stat(text: &str, allowed: &[usize]) -> Reading {
    let mut times = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let mut fields = rest.split_ascii_whitespace();
        let Some(Ok(cpu)) = fields.next().map(str::parse::<usize>) else {
            continue;
        };
        if !allowed.contains(&cpu) {
            continue;
        }
        let ticks: Vec<u64> = fields
            .take(8)
            .map_while(|field| field.parse().ok())
            .collect();
        // user, nice, system and idle are the four columns every kernel has.
        if ticks.len() < 4 {
            continue;
        }
        let iowait = ticks.get(4).copied().unwrap_or(0);
        times.insert(
            cpu,
            CpuTime {
                idle: ticks[3] + iowait,
                total: ticks.iter().sum(),
            },
        );
    }
    times
}

/// The allowed CPUs to draw from: the idle ones, or the least busy when none
/// was idle. `None` when the readings cover none of `allowed`.
fn candidates(allowed: &[usize], before: &Reading, after: &Reading) -> Option<(Vec<usize>, Basis)> {
    // (cpu, busy ticks, total ticks) over the sample.
    let busy: Vec<(usize, u64, u64)> = allowed
        .iter()
        .filter_map(|cpu| {
            let (before, after) = (before.get(cpu)?, after.get(cpu)?);
            let total = after.total.checked_sub(before.total)?;
            let idle = after.idle.checked_sub(before.idle)?;
            (total > 0).then(|| (*cpu, total.saturating_sub(idle), total))
        })
        .collect();
    if busy.is_empty() {
        return None;
    }
    let sampled = busy.len();
    let idle: Vec<usize> = busy
        .iter()
        .filter(|(_, busy, total)| busy * IDLE_DENOMINATOR <= *total)
        .map(|(cpu, ..)| *cpu)
        .collect();
    if !idle.is_empty() {
        let basis = Basis::Idle {
            idle: idle.len(),
            sampled,
        };
        return Some((idle, basis));
    }
    // Least busy by fraction; compare busy_a/total_a with busy_b/total_b by
    // cross-multiplying, so totals that differ by a rounding tick compare fairly.
    let (_, least_busy, least_total) =
        *busy
            .iter()
            .min_by(|(_, busy_a, total_a), (_, busy_b, total_b)| {
                (busy_a * total_b).cmp(&(busy_b * total_a))
            })?;
    let tied: Vec<usize> = busy
        .iter()
        .filter(|(_, busy, total)| busy * least_total == least_busy * total)
        .map(|(cpu, ..)| *cpu)
        .collect();
    let basis = Basis::LeastBusy {
        tied: tied.len(),
        sampled,
    };
    Some((tied, basis))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/proc/stat` with the aggregate line, two full CPU lines, one short
    /// pre-2.6.11 line and the lines that follow the CPUs.
    const STAT: &str = "cpu  100 0 50 1000 10 0 0 0 0 0\n\
                        cpu0 10 2 5 100 4 1 1 0 3 0\n\
                        cpu7 20 0 10 200 0 0 0 0 0 0\n\
                        cpu31 1 2 3 4\n\
                        intr 12345 0 0\n\
                        ctxt 99\n\
                        cpus_allowed 5\n";

    fn times(entries: &[(usize, u64, u64)]) -> Reading {
        entries
            .iter()
            .map(|&(cpu, idle, total)| (cpu, CpuTime { idle, total }))
            .collect()
    }

    #[test]
    fn parsing_reads_each_numbered_cpu_and_skips_the_aggregate_and_other_lines() {
        let parsed = parse_proc_stat(STAT, &[0, 7, 31, 99]);
        // cpu0: idle 100 + iowait 4; total user..steal = 10+2+5+100+4+1+1+0,
        // without the guest column, which user already counts.
        assert_eq!(
            parsed,
            times(&[(0, 104, 123), (7, 200, 230), (31, 4, 10)]),
            "the aggregate `cpu` line must not be read as a CPU numbered 100"
        );
    }

    #[test]
    fn parsing_keeps_only_allowed_cpus() {
        assert_eq!(parse_proc_stat(STAT, &[7]), times(&[(7, 200, 230)]));
        assert!(parse_proc_stat(STAT, &[]).is_empty());
    }

    #[test]
    fn a_cpu_running_a_cpu_bound_task_is_never_a_candidate_beside_idle_ones() {
        // Five ticks each: CPU 3 busy throughout, CPU 5 idle, CPU 9 one
        // rounding tick busy, which still counts as idle.
        let before = times(&[(3, 0, 0), (5, 0, 0), (9, 0, 0)]);
        let after = times(&[(3, 0, 5), (5, 5, 5), (9, 4, 5)]);
        assert_eq!(
            candidates(&[3, 5, 9], &before, &after),
            Some((
                vec![5, 9],
                Basis::Idle {
                    idle: 2,
                    sampled: 3
                }
            ))
        );
    }

    #[test]
    fn with_no_idle_cpu_the_least_busy_fraction_wins_and_ties_are_all_kept() {
        // CPU 1: 4 of 5 busy; CPU 2: 2 of 5; CPU 4: 2 of 5; CPU 6: 3 of 6.
        let before = times(&[(1, 0, 0), (2, 0, 0), (4, 0, 0), (6, 0, 0)]);
        let after = times(&[(1, 1, 5), (2, 3, 5), (4, 3, 5), (6, 3, 6)]);
        assert_eq!(
            candidates(&[1, 2, 4, 6], &before, &after),
            Some((
                vec![2, 4],
                Basis::LeastBusy {
                    tied: 2,
                    sampled: 4
                }
            ))
        );
    }

    #[test]
    fn cpus_the_readings_do_not_cover_are_left_out_and_no_coverage_gives_none() {
        let before = times(&[(5, 0, 0)]);
        let after = times(&[(5, 5, 5), (8, 5, 5)]);
        // CPU 8 has no first reading and CPU 11 no reading at all.
        assert_eq!(
            candidates(&[5, 8, 11], &before, &after),
            Some((
                vec![5],
                Basis::Idle {
                    idle: 1,
                    sampled: 1
                }
            ))
        );
        assert_eq!(candidates(&[8, 11], &before, &after), None);
        // A counter that went backwards, or did not move, is no reading.
        let backwards = times(&[(5, 0, 0)]);
        assert_eq!(candidates(&[5], &after, &backwards), None);
        assert_eq!(candidates(&[5], &before, &before), None);
    }

    #[test]
    fn every_draw_over_a_sparse_mask_is_an_idle_member_of_it() {
        // CPU 31 runs a CPU-bound task; 7 and 211 are idle.
        let before = times(&[(7, 0, 0), (31, 0, 0), (211, 0, 0)]);
        let after = times(&[(7, 5, 5), (31, 0, 5), (211, 5, 5)]);
        let mut drawn = BTreeMap::new();
        for _ in 0..128 {
            let (cpu, basis) =
                choose_with(&[7, 31, 211], |_| Some((before.clone(), after.clone())))
                    .expect("non-empty mask");
            assert_eq!(
                basis,
                Basis::Idle {
                    idle: 2,
                    sampled: 3
                }
            );
            *drawn.entry(cpu).or_insert(0) += 1;
        }
        assert!(!drawn.contains_key(&31), "drew the busy CPU: {drawn:?}");
        assert_eq!(
            drawn.keys().copied().collect::<Vec<_>>(),
            [7, 211],
            "128 uniform draws from two idle CPUs reach both"
        );
    }

    #[test]
    fn without_a_reading_every_allowed_cpu_stays_a_candidate() {
        for _ in 0..128 {
            let (cpu, basis) = choose_with(&[7, 31, 211], |_| None).expect("non-empty mask");
            assert!([7, 31, 211].contains(&cpu), "chose CPU {cpu}");
            assert_eq!(basis, Basis::Unsampled);
        }
    }

    #[test]
    fn choose_returns_a_member_of_the_allowed_set_or_none_when_it_is_empty() {
        assert_eq!(choose(&[]), None);
        assert_eq!(choose(&[42]), Some((42, Basis::OnlyAllowed)));
        let allowed = cpu_ids_from_this_host();
        let (cpu, basis) = choose(&allowed).expect("this process has allowed CPUs");
        assert!(
            allowed.contains(&cpu),
            "chose CPU {cpu} from {allowed:?} ({basis})"
        );
    }

    /// The CPUs this test process may run on.
    fn cpu_ids_from_this_host() -> Vec<usize> {
        let mask = nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0))
            .expect("sched_getaffinity of this process");
        (0..nix::sched::CpuSet::count())
            .filter(|cpu| mask.is_set(*cpu).unwrap_or(false))
            .collect()
    }
}
