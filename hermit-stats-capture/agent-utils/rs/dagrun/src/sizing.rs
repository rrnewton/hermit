//! Memory-footprint modeling and safe concurrency selection.

// Memory footprint model + memory-aware active-step selection (pure functions over a [`DagConfig`]).
//
// Direct port of `py/dagrun/sizing.py`. Rather than a flat per-job RAM estimate, it enumerates
// which steps can actually co-run (no transitive dependency between them, and their summed
// scarce-resource demand fits the caps) and takes the worst-case sum of their per-step memory
// caps. Within a bounded subset-search budget this is exact; wider searches use a conservative
// largest-caps upper bound so sizing cannot become exponential. The chosen ceiling and footprint
// MUST equal the Python build's for the same DAG (cross-tested).

use std::collections::{HashMap, HashSet};

use crate::model::{effective_cpu_count, step_classification, DagConfig, Step, StepClass};

const MAX_EXACT_MEM_COMBINATIONS: u128 = 100_000;

fn scaled_for_width_i64(cap: i64, inner_jobs: i64) -> i64 {
    let scaled = (cap as i128 * inner_jobs as i128) / 4;
    scaled.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn scaled_between_widths_i64(cap: i64, inner_jobs: i64, baseline_jobs: i64) -> i64 {
    let scaled = (cap as i128 * inner_jobs as i128) / baseline_jobs as i128;
    scaled.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Resolve the inner-cgroup memory cap for a step.
///
/// An explicit hard cap wins. Otherwise the RSS baseline is multiplied by `mem_cap_factor`; a step
/// with neither value receives `default_cap_bytes` when supplied.
pub fn step_mem_cap_bytes(
    step: &Step,
    mem_cap_factor: f64,
    default_cap_bytes: Option<i64>,
) -> Option<i64> {
    if let Some(hard) = step.hint.hard_mem_max_bytes.filter(|value| *value > 0) {
        return Some(hard);
    }
    match step.hint.rss_baseline_bytes {
        Some(base) if base > 0 && mem_cap_factor.is_finite() && mem_cap_factor > 0.0 => {
            Some(((base as f64 * mem_cap_factor) as i64).max(1))
        }
        _ => default_cap_bytes.filter(|value| *value > 0),
    }
}

/// Per-step cap scaled for internal parallelism.
///
/// Conservative `P x J` model: an explicit hard cap and non-CPU-bound steps keep the base
/// cap; CPU-bound steps scale linearly above J=4.
pub fn step_mem_cap_for_inner_jobs(
    step: &Step,
    inner_jobs: Option<i64>,
    mem_cap_factor: f64,
) -> i64 {
    // Active-step sizing excludes uncharacterized steps (no default here), matching Python.
    step_mem_cap_for_inner_jobs_optional(step, inner_jobs, mem_cap_factor, None).unwrap_or(0)
}

/// Width-scaled cap while preserving an absent cap as `None`.
///
/// Sizing maps an uncharacterized step to zero because it is excluded from the footprint sum;
/// runtime enforcement supplies the DAG's forcing-function default and must retain `None` when
/// that default is disabled. This shared implementation keeps their scaling arithmetic identical.
pub(crate) fn step_mem_cap_for_inner_jobs_optional(
    step: &Step,
    inner_jobs: Option<i64>,
    mem_cap_factor: f64,
    default_cap_bytes: Option<i64>,
) -> Option<i64> {
    let cap = step_mem_cap_bytes(step, mem_cap_factor, default_cap_bytes)?;
    match inner_jobs {
        Some(jobs)
            if step.hint.hard_mem_max_bytes.is_none_or(|value| value <= 0)
                && step_classification(step) == StepClass::CpuBound =>
        {
            if let Some(measured_width) = step
                .hint
                .rss_baseline_inner_jobs
                .filter(|measured_width| *measured_width > 0)
            {
                // An empirical M(p) already describes the complete step at `measured_width`.
                // Keep it unchanged at that (or a narrower) width and extrapolate only if a later
                // caller widens beyond the measured point.
                if jobs <= measured_width {
                    return Some(cap);
                }
                return Some(cap.max(scaled_between_widths_i64(cap, jobs, measured_width)));
            }
            // Exact integer arithmetic, truncated toward zero and saturated to i64. Python uses
            // the same operation, avoiding binary64 drift above 2^53.
            let scaled = scaled_for_width_i64(cap, jobs);
            Some(cap.max(scaled))
        }
        _ => Some(cap),
    }
}

/// Map each step tag to all transitive dependencies using an explicit deterministic DFS stack.
///
/// This cannot overflow the call stack on a reverse-topological chain thousands of nodes deep.
pub fn transitive_deps(steps: &[Step]) -> HashMap<String, HashSet<String>> {
    let direct: HashMap<String, Vec<String>> =
        steps.iter().map(|s| (s.tag(), s.deps.clone())).collect();
    let mut result: HashMap<String, HashSet<String>> = HashMap::new();
    for step in steps {
        let tag = step.tag();
        let mut deps: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = direct
            .get(&tag)
            .into_iter()
            .flatten()
            .rev()
            .cloned()
            .collect();
        while let Some(dep) = stack.pop() {
            if !deps.insert(dep.clone()) {
                continue;
            }
            if let Some(next) = direct.get(&dep) {
                stack.extend(next.iter().rev().cloned());
            }
        }
        result.insert(tag, deps);
    }
    result
}

// Generate all `k`-combinations of the indices `0..n` (in lexicographic order), matching
// Python's `itertools.combinations`.
fn combinations(n: usize, k: usize) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    if k == 0 || k > n {
        if k == 0 {
            out.push(Vec::new());
        }
        return out;
    }
    let mut idx: Vec<usize> = (0..k).collect();
    loop {
        out.push(idx.clone());
        // Find the rightmost index that can be incremented.
        let mut i = k;
        loop {
            if i == 0 {
                return out;
            }
            i -= 1;
            if idx[i] != i + n - k {
                break;
            }
        }
        idx[i] += 1;
        for j in i + 1..k {
            idx[j] = idx[j - 1] + 1;
        }
    }
}

fn too_many_combinations(n: usize, width: usize) -> bool {
    let mut term: u128 = 1;
    let mut total: u128 = 0;
    for count in 1..=width {
        term = term.saturating_mul((n - count + 1) as u128) / count as u128;
        total = total.saturating_add(term);
        if total > MAX_EXACT_MEM_COMBINATIONS {
            return true;
        }
    }
    false
}

/// Maximum per-step-cap sum over any scheduler-reachable concurrent set of size `<= jobs`.
///
/// A set is reachable only when no member transitively depends on another and the summed
/// scarce-resource demand fits `cfg.resource_caps`. Every runnable non-skipped step participates:
/// a hard cap wins, an RSS baseline derives a cap, and an undeclared step uses the configured
/// default cap. An uncharacterized step with that default disabled is conservatively unbounded.
/// Returns `(best_total, chosen_tags)`. `inner_jobs` applies ONE internal-parallelism width to every
/// step; when absent, each step's effective preferred/default width is used so ordinary
/// `--max-mem` sizing reflects the post-plan configuration.
pub fn schedulable_peak_mem_bytes(
    cfg: &DagConfig,
    jobs: i64,
    inner_jobs: Option<i64>,
) -> (i64, Vec<String>) {
    peak_mem_over_sets(cfg, jobs, &|s| {
        let width = inner_jobs.or_else(|| effective_cpu_count(s, cfg.default_step_cpu_count));
        step_mem_cap_for_inner_jobs_optional(
            s,
            width,
            cfg.mem_cap_factor,
            cfg.default_step_mem_cap_bytes,
        )
        .unwrap_or(i64::MAX)
    })
}

/// Compute peak schedulable memory using a separate inner-job width for each step.
///
/// A step absent from `widths` falls back to its effective preferred/default width.
pub fn schedulable_peak_mem_bytes_widths(
    cfg: &DagConfig,
    jobs: i64,
    widths: &HashMap<String, i64>,
) -> (i64, Vec<String>) {
    peak_mem_over_sets(cfg, jobs, &|s| {
        let width = widths
            .get(&s.tag())
            .copied()
            .or_else(|| effective_cpu_count(s, cfg.default_step_cpu_count));
        step_mem_cap_for_inner_jobs_optional(
            s,
            width,
            cfg.mem_cap_factor,
            cfg.default_step_mem_cap_bytes,
        )
        .unwrap_or(i64::MAX)
    })
}

/// Shared core of [`schedulable_peak_mem_bytes`] / [`schedulable_peak_mem_bytes_widths`]: the
/// reachable-concurrent-set enumeration, parameterized by how a participating step's memory cap is
/// computed (`cap_of`). DRY: the two public entry points differ only in that closure.
fn peak_mem_over_sets(
    cfg: &DagConfig,
    jobs: i64,
    cap_of: &dyn Fn(&Step) -> i64,
) -> (i64, Vec<String>) {
    // Participating steps keep cfg order (mirrors Python's insertion-ordered `by_tag`); `tags`
    // and `participating` are parallel, so a combination index selects both at once.
    let participating: Vec<&Step> = cfg
        .steps
        .iter()
        .filter(|s| s.skip_reason.is_none())
        .collect();
    let tags: Vec<String> = participating.iter().map(|s| s.tag()).collect();
    let width = jobs.max(1).min(tags.len() as i64) as usize;
    let mut best_total: i64 = 0;
    let mut best: Vec<String> = Vec::new();

    if width == 0 {
        return (0, Vec::new());
    }
    if width == 1 {
        // No dependency closure is needed when only one step may run. Update only on a STRICTLY
        // larger cap so equal maxima retain authored cfg order, matching Python's stable `max`.
        let mut chosen_index = 0usize;
        let mut chosen_cap = cap_of(participating[0]);
        for (index, step) in participating.iter().enumerate().skip(1) {
            let cap = cap_of(step);
            if cap > chosen_cap {
                chosen_index = index;
                chosen_cap = cap;
            }
        }
        return (chosen_cap, vec![tags[chosen_index].clone()]);
    }

    // Exact antichain/resource enumeration is exponential. Above the shared fixed search budget,
    // conservatively ignore dependencies/resources and sum the largest `jobs` caps. This can only
    // overestimate reachable memory, never admit unsafe concurrency. Original cfg order breaks
    // equal-cap ties deterministically, matching Python's stable sort.
    if too_many_combinations(tags.len(), width) {
        let mut ranked: Vec<(usize, i64)> = participating
            .iter()
            .enumerate()
            .map(|(index, step)| (index, cap_of(step)))
            .collect();
        ranked.sort_by(|(left_index, left_cap), (right_index, right_cap)| {
            right_cap
                .cmp(left_cap)
                .then_with(|| left_index.cmp(right_index))
        });
        let chosen_indices: Vec<usize> = ranked
            .into_iter()
            .take(width)
            .map(|(index, _)| index)
            .collect();
        let total = chosen_indices
            .iter()
            .map(|index| cap_of(participating[*index]))
            .fold(0, i64::saturating_add);
        let chosen = chosen_indices
            .into_iter()
            .map(|index| tags[index].clone())
            .collect();
        return (total, chosen);
    }

    let dependencies = transitive_deps(&cfg.steps);
    for count in 1..=width {
        for combo in combinations(tags.len(), count) {
            // No two members may have a (transitive) dependency relation, either direction.
            let mut dependent = false;
            'pairs: for a in 0..combo.len() {
                for b in (a + 1)..combo.len() {
                    let left = &tags[combo[a]];
                    let right = &tags[combo[b]];
                    let l_in_r = dependencies.get(right).is_some_and(|d| d.contains(left));
                    let r_in_l = dependencies.get(left).is_some_and(|d| d.contains(right));
                    if l_in_r || r_in_l {
                        dependent = true;
                        break 'pairs;
                    }
                }
            }
            if dependent {
                continue;
            }
            // Summed scarce-resource demand must fit every cap.
            let over_cap = cfg.resource_caps.iter().any(|(resource, cap)| {
                let sum: i64 = combo
                    .iter()
                    .map(|&i| {
                        participating[i]
                            .hint
                            .resources
                            .get(resource)
                            .copied()
                            .unwrap_or(0)
                    })
                    .fold(0, i64::saturating_add);
                sum > *cap
            });
            if over_cap {
                continue;
            }
            let total: i64 = combo
                .iter()
                .map(|&i| cap_of(participating[i]))
                .fold(0, i64::saturating_add);
            if total > best_total {
                best_total = total;
                best = combo.iter().map(|&i| tags[i].clone()).collect();
            }
        }
    }
    (best_total, best)
}

/// Worst-case footprint (bytes) at the given `-j`, clamped to the configured floor.
pub fn jobs_footprint_bytes(cfg: &DagConfig, jobs: i64, inner_jobs: Option<i64>) -> i64 {
    let (peak, _) = schedulable_peak_mem_bytes(cfg, jobs, inner_jobs);
    outer_mem_footprint_bytes(cfg, peak)
}

/// Apply the run-level floor and safety factor to one modeled peak.
pub(crate) fn outer_mem_footprint_bytes(cfg: &DagConfig, peak: i64) -> i64 {
    if peak == i64::MAX {
        return i64::MAX;
    }
    let scaled = if cfg.outer_mem_safety_factor.is_finite() && cfg.outer_mem_safety_factor > 0.0 {
        if peak > 0 {
            ((peak as f64 * cfg.outer_mem_safety_factor) as i64).max(1)
        } else {
            0
        }
    } else {
        i64::MAX
    };
    cfg.mem_cap_floor_bytes.max(0).max(scaled)
}

/// Whether a finite, non-overflowed footprint fits a finite budget.
pub(crate) fn memory_footprint_fits(footprint: i64, budget: i64) -> bool {
    (0..i64::MAX).contains(&footprint) && footprint <= budget
}

/// Largest active-step ceiling, capped at CPU count, whose worst-case footprint fits `budget`.
/// Returns `(0, one_step_footprint)` when even one runnable step or the configured floor cannot
/// fit; callers must refuse rather than execute an infeasible graph.
pub fn jobs_for_budget(cfg: &DagConfig, budget: i64) -> (i64, i64) {
    let ncpu = cpu_count();
    let one = jobs_footprint_bytes(cfg, 1, None);
    if !memory_footprint_fits(one, budget) {
        return (0, one);
    }
    let mut best: i64 = 1;
    for n in 1..=ncpu {
        if memory_footprint_fits(jobs_footprint_bytes(cfg, n, None), budget) {
            best = n;
        } else {
            break; // footprint is monotonic non-decreasing in n
        }
    }
    (best, jobs_footprint_bytes(cfg, best, None))
}

/// Conservative peak resident-memory allowance for one build worker.
pub const PER_BUILD_JOB_MEM_BYTES: i64 = 1024 * 1024 * 1024;

/// Derive a positive build-worker count from co-located CPU and memory limits.
///
/// The result is the smaller of granted cores and the number of conservative per-worker memory
/// allowances that fit the cap. Missing limits fall back to the host CPU count.
pub fn derive_build_jobs(cpu_count: Option<i64>, mem_max_bytes: Option<i64>) -> i64 {
    let cores = match cpu_count {
        Some(c) if c > 0 => c,
        // Full path: the `cpu_count` parameter shadows the module fn of the same name.
        _ => crate::sizing::cpu_count(),
    };
    let mut jobs = cores;
    if let Some(m) = mem_max_bytes {
        if m > 0 {
            jobs = jobs.min(m / PER_BUILD_JOB_MEM_BYTES);
        }
    }
    jobs.max(1)
}

/// The build-width variable the runner controls. cargo reads it, and the `NUM_JOBS` it exports to
/// build scripts follows it.
pub const BUILD_JOBS_ENV: &str = "CARGO_BUILD_JOBS";

/// The operator's own build width, carried ACROSS the runner's systemd re-exec.
///
/// This exists because `CARGO_BUILD_JOBS` alone cannot answer "did a human ask for this?". The
/// runner writes that variable itself — `attempt_scope_reexec` passes
/// `--setenv=CARGO_BUILD_JOBS=...` into the scope — so the in-scope process reads back the
/// runner's own derivation and would mistake it for an instruction. Then per-step downward
/// refinement stops, every step keeps the whole scope's width, and that is exactly the
/// 284-wide-against-8-GiB condition this machinery exists to prevent.
///
/// So intent is resolved ONCE, in the outermost process, from the truly ambient environment, and
/// passed forward under its own name. PRESENCE of this variable means "already resolved"; an
/// empty value means "resolved, and the operator asked for nothing".
pub const OPERATOR_BUILD_JOBS_ENV: &str = "DAGRUN_OPERATOR_BUILD_JOBS";

/// A build width an operator can be said to have CHOSEN: a positive decimal integer.
///
/// Absent, empty, malformed, or non-positive is `None` — not an instruction. A zero or a typo
/// read as intent would hand the whole run a width nobody picked, which is worse than falling
/// back to a derivation whose reasoning is stated.
pub fn parse_build_jobs(raw: Option<&str>) -> Option<i64> {
    let text = raw?.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<i64>().ok().filter(|value| *value > 0)
}

/// Resolve operator intent from the two variables, without touching the environment.
///
/// `forwarded` is `DAGRUN_OPERATOR_BUILD_JOBS` and `ambient` is `CARGO_BUILD_JOBS`. PRESENCE of
/// the first wins outright, empty included: an outer runner already asked this question of the
/// real environment, and its answer — including "the operator asked for nothing" — is the only
/// one that is still trustworthy, because that same runner has since written `ambient` itself.
pub fn resolve_operator_build_jobs(forwarded: Option<&str>, ambient: Option<&str>) -> Option<i64> {
    match forwarded {
        Some(value) => parse_build_jobs(Some(value)),
        None => parse_build_jobs(ambient),
    }
}

/// The build width the operator chose, or `None` if they expressed none.
///
/// Read once and memoized: this process must answer the same way before and after it has written
/// anything into a child's environment.
pub fn operator_build_jobs() -> Option<i64> {
    static CAPTURED: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
    *CAPTURED.get_or_init(|| {
        let forwarded = std::env::var(OPERATOR_BUILD_JOBS_ENV).ok();
        let ambient = std::env::var(BUILD_JOBS_ENV).ok();
        resolve_operator_build_jobs(forwarded.as_deref(), ambient.as_deref())
    })
}

/// Which build width governs, what the alternative was, and why — so an OOM is explicable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildJobsChoice {
    /// The width that will actually be exported.
    pub jobs: i64,
    /// What the containment derivation would have chosen from the caps in force.
    pub derived: i64,
    /// The operator's stated width, or `None` when they stated none.
    pub operator: Option<i64>,
}

impl BuildJobsChoice {
    /// `"operator"` or `"containment"`.
    pub fn source(&self) -> &'static str {
        if self.operator.is_some() {
            "operator"
        } else {
            "containment"
        }
    }

    /// One sentence naming the winner, the loser, and the consequence.
    pub fn describe(&self) -> String {
        match self.operator {
            Some(chosen) => format!(
                "build width: honouring {BUILD_JOBS_ENV}={chosen} from the environment; the \
                 containment default would have chosen {}. Per-step downward refinement is OFF \
                 for this run, so a memory cap sized for a narrower pool can still OOM.",
                self.derived
            ),
            None => format!(
                "build width: no {BUILD_JOBS_ENV} in the environment, so the containment default \
                 governs at {} for this scope, refined downward per step.",
                self.derived
            ),
        }
    }
}

/// Choose the build width for a scope or a step, and record what lost.
///
/// A cgroup quota is a CEILING, not a parallelism instruction. When an operator has stated a
/// width — having, presumably, sized their memory cap against exactly that pool — the derived
/// number does not get to replace it silently. When they have stated nothing, the derivation
/// governs and stays free to narrow per step.
pub fn choose_build_jobs(
    operator: Option<i64>,
    cpu_count: Option<i64>,
    mem_max_bytes: Option<i64>,
) -> BuildJobsChoice {
    let derived = derive_build_jobs(cpu_count, mem_max_bytes);
    BuildJobsChoice {
        jobs: operator.unwrap_or(derived),
        derived,
        operator,
    }
}

/// [`choose_build_jobs`] against this process's captured operator intent.
pub fn select_build_jobs(cpu_count: Option<i64>, mem_max_bytes: Option<i64>) -> BuildJobsChoice {
    choose_build_jobs(operator_build_jobs(), cpu_count, mem_max_bytes)
}

/// Return the online logical CPU count, independent of process affinity.
///
/// Reads the Linux online-CPU range first, then falls back to runtime parallelism and finally four.
pub fn cpu_count() -> i64 {
    if let Ok(text) = std::fs::read_to_string("/sys/devices/system/cpu/online") {
        if let Some(n) = count_cpu_ranges(text.trim()) {
            return n;
        }
    }
    if let Ok(n) = std::thread::available_parallelism() {
        return n.get() as i64;
    }
    4
}

/// Count CPUs from a Linux cpu-range list like `"0-3,5,7-8"`.
///
/// Shared with `perflog::affinity_width` (which parses `/proc/self/status` `Cpus_allowed_list`),
/// so both the `-j` sizing default and the perf-log `nproc()` use one CPU-range parser.
pub(crate) fn count_cpu_ranges(spec: &str) -> Option<i64> {
    if spec.is_empty() {
        return None;
    }
    let mut total: i64 = 0;
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((lo, hi)) => {
                let lo: i64 = lo.trim().parse().ok()?;
                let hi: i64 = hi.trim().parse().ok()?;
                if hi < lo {
                    return None;
                }
                total += hi - lo + 1;
            }
            None => {
                part.trim().parse::<i64>().ok()?;
                total += 1;
            }
        }
    }
    if total > 0 {
        Some(total)
    } else {
        None
    }
}

/// Parse a non-negative byte-size string with an optional binary `K`, `M`, `G`, or `T` suffix.
///
/// Decimal quantities, surrounding whitespace, and an optional trailing `B` are accepted. Invalid
/// or overflowing values return `None`.
pub fn parse_size(spec: &str) -> Option<i64> {
    if spec.is_empty() {
        return None;
    }
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0usize;
    let n = chars.len();
    let skip_ws = |i: &mut usize| {
        while *i < n && chars[*i].is_whitespace() {
            *i += 1;
        }
    };

    skip_ws(&mut i);
    // Integer part: one or more ASCII digits.
    let start = i;
    while i < n && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None; // needs at least one digit
    }
    // Optional fractional part: '.' followed by one or more digits.
    if i < n && chars[i] == '.' {
        let frac_start = i + 1;
        let mut j = frac_start;
        while j < n && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j == frac_start {
            return None; // '.' with no digits after is not a match
        }
        i = j;
    }
    let number: String = chars[start..i].iter().collect();
    let value: f64 = number.parse().ok()?;

    skip_ws(&mut i);
    // Optional single size unit.
    let mult: f64 = if i < n {
        match chars[i] {
            'K' | 'k' => {
                i += 1;
                1024.0
            }
            'M' | 'm' => {
                i += 1;
                1024f64.powi(2)
            }
            'G' | 'g' => {
                i += 1;
                1024f64.powi(3)
            }
            'T' | 't' => {
                i += 1;
                1024f64.powi(4)
            }
            _ => 1.0,
        }
    } else {
        1.0
    };
    // Optional trailing B/b (captured but ignored by Python).
    if i < n && (chars[i] == 'B' || chars[i] == 'b') {
        i += 1;
    }
    skip_ws(&mut i);
    if i != n {
        return None; // trailing junk -> no fullmatch
    }
    Some((value * mult) as i64)
}

/// Bytes currently allocatable without swapping (`MemAvailable`), or `None` if unreadable.
pub fn mem_available_bytes() -> Option<i64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: i64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// The current cgroup-v2 memory ceiling, or `None` when unbounded/unreadable.
pub fn cgroup_mem_max_bytes() -> Option<i64> {
    let raw = std::fs::read_to_string("/sys/fs/cgroup/memory.max").ok()?;
    let text = raw.trim();
    if text == "max" {
        None
    } else {
        text.parse().ok()
    }
}

/// Conservative memory budget available to a stress fan-out.
pub fn box_mem_budget_bytes() -> Option<i64> {
    [cgroup_mem_max_bytes(), mem_available_bytes()]
        .into_iter()
        .flatten()
        .min()
}

/// Conservative footprint of one complete stress copy.
///
/// Every active non-engine-only step is charged its full inner cap; undeclared steps receive the
/// same small default cap used by the scheduler. Intentional pre-execution skips are excluded.
/// Summing the graph is deliberately an upper bound.
pub fn stress_copy_footprint_bytes(cfg: &DagConfig, default_step_bytes: Option<i64>) -> i64 {
    let default_step_bytes = default_step_bytes
        .filter(|value| *value > 0)
        .or(cfg.default_step_mem_cap_bytes)
        .filter(|value| *value > 0);
    let control_floor = stress_control_floor_bytes(cfg, default_step_bytes);
    let mut total = 0i64;
    let mut runnable = 0usize;
    for step in cfg.steps.iter().filter(|step| step.skip_reason.is_none()) {
        runnable += 1;
        let cap = step_mem_cap_for_inner_jobs_optional(
            step,
            effective_cpu_count(step, cfg.default_step_cpu_count),
            cfg.mem_cap_factor,
            default_step_bytes,
        )
        .unwrap_or(i64::MAX);
        total = total.saturating_add(cap);
    }
    if runnable > 0 {
        total.max(control_floor)
    } else {
        control_floor
    }
}

/// Minimum control-plane memory charged to each complete stress graph copy.
///
/// A positive configured/explicit default is the conservative SMALL forcing-function allowance
/// (normally 1 GiB). With that default deliberately disabled, the configured memory floor or one
/// byte preserves finite hard-cap models. The CLI separately enforces a generated-node cap, which
/// deterministically bounds config-object allocation.
pub(crate) fn stress_control_floor_bytes(cfg: &DagConfig, default_step_bytes: Option<i64>) -> i64 {
    default_step_bytes
        .filter(|value| *value > 0)
        .or(cfg.default_step_mem_cap_bytes)
        .filter(|value| *value > 0)
        .unwrap_or_else(|| cfg.mem_cap_floor_bytes.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ResourceHint;
    use std::collections::BTreeMap;

    const GIB: i64 = 1024 * 1024 * 1024;

    fn step(group: &str, job: &str, rss: Option<i64>, deps: &[&str], gpu: bool) -> Step {
        let mut resources = BTreeMap::new();
        if gpu {
            resources.insert("gpu".to_string(), 1);
        }
        Step {
            group: group.into(),
            job: job.into(),
            desc: String::new(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".into(),
            cmdtype: crate::model::CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: deps.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            hint: ResourceHint {
                resources,
                rss_baseline_bytes: rss,
                ..Default::default()
            },
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: 1800,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        }
    }

    fn cfg() -> DagConfig {
        let mut caps = BTreeMap::new();
        caps.insert("gpu".to_string(), 1);
        DagConfig {
            steps: vec![
                step("g", "A", Some(3 * GIB), &[], false),
                step("g", "B", Some(2 * GIB), &["g.A"], false),
                step("g", "C", Some(4 * GIB), &[], true),
                step("g", "D", Some(GIB), &[], true),
            ],
            description: String::new(),
            resource_caps: caps,
            mem_cap_factor: 1.0,
            outer_mem_safety_factor: 1.0,
            mem_cap_floor_bytes: 0,
            default_step_timeout: 1800,
            default_jobs_flag: "-j".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn parse_size_cases() {
        assert_eq!(parse_size("8G"), Some(8 * GIB));
        assert_eq!(parse_size("4096M"), Some(4096 * 1024 * 1024));
        assert_eq!(parse_size("2048K"), Some(2048 * 1024));
        assert_eq!(parse_size("12345"), Some(12345));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("nonsense"), None);
        // whitespace + trailing B forms Python's regex accepts.
        assert_eq!(parse_size(" 8 GB "), Some(8 * GIB));
    }

    #[test]
    fn stress_footprint_charges_every_step() {
        let mut value = cfg();
        value.steps.push(step("g", "E", None, &[], false));
        assert_eq!(
            stress_copy_footprint_bytes(&value, Some(GIB)),
            3 * GIB + 2 * GIB + 4 * GIB + GIB + GIB
        );
    }

    #[test]
    fn stress_footprint_has_the_default_floor() {
        let mut value = cfg();
        value.steps.clear();
        assert_eq!(stress_copy_footprint_bytes(&value, Some(GIB)), GIB);
        value.steps.push(step("g", "tiny", None, &[], false));
        value.steps[0].hint.hard_mem_max_bytes = Some(1);
        assert_eq!(stress_copy_footprint_bytes(&value, Some(GIB)), GIB);
    }

    #[test]
    fn stress_without_default_only_marks_uncharacterized_steps_unbounded() {
        let mut hard = step("g", "hard", None, &[], false);
        hard.hint.hard_mem_max_bytes = Some(2 * GIB);
        let rss = step("g", "rss", Some(3 * GIB), &[], false);
        let characterized = DagConfig {
            steps: vec![hard, rss],
            mem_cap_factor: 1.0,
            default_step_mem_cap_bytes: None,
            mem_cap_floor_bytes: 0,
            ..Default::default()
        };
        assert_eq!(stress_copy_footprint_bytes(&characterized, None), 5 * GIB);

        let uncharacterized = DagConfig {
            steps: vec![step("g", "bare", None, &[], false)],
            default_step_mem_cap_bytes: None,
            ..Default::default()
        };
        assert_eq!(
            stress_copy_footprint_bytes(&uncharacterized, None),
            i64::MAX
        );
    }

    #[test]
    fn empty_stress_footprint_uses_default_then_floor() {
        let with_default = DagConfig {
            steps: vec![],
            default_step_mem_cap_bytes: Some(GIB),
            mem_cap_floor_bytes: 2 * GIB,
            ..Default::default()
        };
        assert_eq!(stress_copy_footprint_bytes(&with_default, None), GIB);
        let floor_only = DagConfig {
            default_step_mem_cap_bytes: None,
            ..with_default
        };
        assert_eq!(stress_copy_footprint_bytes(&floor_only, None), 2 * GIB);
    }

    #[test]
    fn transitive_deps_cases() {
        let deps = transitive_deps(&cfg().steps);
        assert_eq!(deps["g.B"], HashSet::from(["g.A".to_string()]));
        assert!(deps["g.A"].is_empty());
    }

    #[test]
    fn transitive_deps_handles_1100_node_reverse_chain_iteratively() {
        let steps: Vec<Step> = (0..1100)
            .rev()
            .map(|index| {
                let dep = (index > 0).then(|| format!("chain.s{}", index - 1));
                let mut value = step("chain", &format!("s{index}"), None, &[], false);
                value.deps = dep.into_iter().collect();
                value
            })
            .collect();
        let deps = transitive_deps(&steps);
        assert_eq!(deps["chain.s1099"].len(), 1099);
        assert!(deps["chain.s1099"].contains("chain.s0"));
        assert!(deps["chain.s0"].is_empty());

        let value = DagConfig {
            steps,
            default_step_mem_cap_bytes: Some(1),
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            ..Default::default()
        };
        assert_eq!(schedulable_peak_mem_bytes(&value, 1100, None).0, 1100);
    }

    #[test]
    fn width_one_sizing_skips_closure_on_5000_node_reverse_chain() {
        let steps: Vec<Step> = (0..5000)
            .rev()
            .map(|index| {
                let dep = (index > 0).then(|| format!("wide.s{}", index - 1));
                let mut value = step("wide", &format!("s{index}"), None, &[], false);
                value.deps = dep.into_iter().collect();
                value.hint.hard_mem_max_bytes =
                    Some(if index == 4999 || index == 4000 { 2 } else { 1 });
                value
            })
            .collect();
        let value = DagConfig {
            steps,
            ..Default::default()
        };
        assert_eq!(
            schedulable_peak_mem_bytes(&value, 1, None),
            (2, vec!["wide.s4999".to_string()])
        );
    }

    #[test]
    fn step_mem_cap_hard_override_wins() {
        let mut hint = ResourceHint {
            rss_baseline_bytes: Some(2 * GIB),
            hard_mem_max_bytes: Some(9 * GIB),
            ..Default::default()
        };
        let _ = &mut hint;
        let s = Step {
            group: "g".into(),
            job: "X".into(),
            desc: String::new(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".into(),
            cmdtype: crate::model::CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: vec![],
            env: BTreeMap::new(),
            hint,
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: 1800,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        };
        assert_eq!(step_mem_cap_bytes(&s, 1.25, None), Some(9 * GIB));
    }

    #[test]
    fn exact_width_baseline_is_not_scaled_twice() {
        let mut hint = ResourceHint {
            rss_baseline_bytes: Some(3 * GIB),
            rss_baseline_inner_jobs: Some(8),
            classification: StepClass::CpuBound,
            ..Default::default()
        };
        let step = Step {
            group: "g".into(),
            job: "measured".into(),
            desc: String::new(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".into(),
            cmdtype: crate::model::CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: vec![],
            env: std::collections::BTreeMap::new(),
            hint: hint.clone(),
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: 1800,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        };
        assert_eq!(step_mem_cap_for_inner_jobs(&step, Some(8), 1.0), 3 * GIB);
        assert_eq!(step_mem_cap_for_inner_jobs(&step, Some(4), 1.0), 3 * GIB);
        assert_eq!(step_mem_cap_for_inner_jobs(&step, Some(16), 1.0), 6 * GIB);

        hint.hard_mem_max_bytes = Some(5 * GIB);
        let mut hard = step;
        hard.hint = hint;
        assert_eq!(step_mem_cap_for_inner_jobs(&hard, Some(16), 1.0), 5 * GIB);
    }

    #[test]
    fn schedulable_peak_picks_best_feasible_set() {
        let (total, chosen) = schedulable_peak_mem_bytes(&cfg(), 4, None);
        assert_eq!(total, 7 * GIB);
        let set: HashSet<String> = chosen.into_iter().collect();
        assert_eq!(set, HashSet::from(["g.A".to_string(), "g.C".to_string()]));
    }

    #[test]
    fn jobs_for_budget_monotonic_and_at_least_one() {
        let c = cfg();
        assert_eq!(jobs_footprint_bytes(&c, 1, None), 4 * GIB);
        assert_eq!(jobs_for_budget(&c, 6 * GIB), (1, 4 * GIB));
        assert_eq!(jobs_for_budget(&c, GIB), (0, 4 * GIB));
    }

    #[test]
    fn jobs_for_budget_scales_each_cpu_bound_steps_effective_width() {
        let mut preferred = step("g", "preferred", Some(GIB), &[], false);
        preferred.hint.classification = StepClass::CpuBound;
        preferred.hint.preferred_inner_jobs = Some(8);
        let mut defaulted = step("g", "defaulted", Some(GIB), &[], false);
        defaulted.hint.classification = StepClass::CpuBound;
        let value = DagConfig {
            steps: vec![preferred, defaulted],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            default_step_cpu_count: Some(8),
            ..Default::default()
        };

        // The width model is linear above j4, so each j8 step costs 2 GiB and the pair costs 4 GiB.
        assert_eq!(schedulable_peak_mem_bytes(&value, 2, None).0, 4 * GIB);
        assert_eq!(jobs_footprint_bytes(&value, 1, None), 2 * GIB);
        assert_eq!(jobs_for_budget(&value, 3 * GIB), (1, 2 * GIB));
    }

    #[test]
    fn memory_classes_and_hard_cap_width_rules() {
        let mut cpu = step("g", "cpu", Some(GIB), &[], false);
        cpu.hint.classification = StepClass::CpuBound;
        let light = step("g", "light", Some(GIB), &[], false);
        let mut hard = step("g", "hard", Some(GIB), &[], false);
        hard.hint.classification = StepClass::CpuBound;
        hard.hint.hard_mem_max_bytes = Some(3 * GIB);

        assert_eq!(step_mem_cap_for_inner_jobs(&cpu, Some(4), 1.0), GIB);
        assert_eq!(step_mem_cap_for_inner_jobs(&cpu, Some(8), 1.0), 2 * GIB);
        assert_eq!(step_mem_cap_for_inner_jobs(&light, Some(8), 1.0), GIB);
        assert_eq!(step_mem_cap_for_inner_jobs(&hard, Some(8), 1.0), 3 * GIB);
    }

    #[test]
    fn sizing_counts_hard_default_and_selected_engine_steps() {
        let mut hard = step("g", "hard", None, &[], false);
        hard.hint.hard_mem_max_bytes = Some(6 * GIB);
        let mut defaulted = step("g", "defaulted", None, &[], false);
        defaulted.hint.classification = StepClass::CpuBound;
        defaulted.engine_only = true;
        let value = DagConfig {
            steps: vec![hard, defaulted],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            default_step_mem_cap_bytes: Some(GIB),
            default_step_cpu_count: Some(8),
            ..Default::default()
        };

        assert_eq!(schedulable_peak_mem_bytes(&value, 2, None).0, 8 * GIB);
        assert_eq!(jobs_for_budget(&value, 5 * GIB), (0, 6 * GIB));
    }

    #[test]
    fn sizing_saturates_i64_instead_of_overflowing() {
        let value = DagConfig {
            steps: vec![
                step("g", "a", Some(i64::MAX), &[], false),
                step("g", "b", Some(i64::MAX), &[], false),
            ],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            ..Default::default()
        };

        assert_eq!(schedulable_peak_mem_bytes(&value, 2, None).0, i64::MAX);
        assert_eq!(jobs_for_budget(&value, i64::MAX), (0, i64::MAX));

        let mut discounted = value.clone();
        discounted.outer_mem_safety_factor = 0.5;
        assert_eq!(jobs_footprint_bytes(&discounted, 2, None), i64::MAX);

        let unknown = DagConfig {
            steps: vec![step("g", "unknown", None, &[], false)],
            default_step_mem_cap_bytes: None,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 0.5,
            ..Default::default()
        };
        assert_eq!(jobs_footprint_bytes(&unknown, 1, None), i64::MAX);
    }

    #[test]
    fn stress_footprint_uses_width_aware_runtime_caps() {
        let mut wide = step("g", "wide", Some(GIB), &[], false);
        wide.hint.classification = StepClass::CpuBound;
        wide.hint.preferred_inner_jobs = Some(8);
        let value = DagConfig {
            steps: vec![wide],
            mem_cap_factor: 1.0,
            ..Default::default()
        };

        assert_eq!(stress_copy_footprint_bytes(&value, None), 2 * GIB);
    }

    #[test]
    fn invalid_nonpositive_memory_hints_fall_back_safely() {
        let mut invalid = step("g", "invalid", Some(0), &[], false);
        invalid.hint.hard_mem_max_bytes = Some(0);
        assert_eq!(step_mem_cap_bytes(&invalid, 1.0, Some(GIB)), Some(GIB));
        let baseline = step("g", "factor", Some(8 * GIB), &[], false);
        assert_eq!(step_mem_cap_bytes(&baseline, 0.0, Some(GIB)), Some(GIB));
        assert_eq!(step_mem_cap_bytes(&baseline, 1e-300, Some(GIB)), Some(1));
        let value = DagConfig {
            steps: vec![baseline],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: -1,
            outer_mem_safety_factor: 0.0,
            ..Default::default()
        };
        assert_eq!(jobs_for_budget(&value, 16 * GIB), (0, i64::MAX));
    }

    #[test]
    fn wide_dag_uses_bounded_conservative_memory_fallback() {
        let mut steps = Vec::new();
        for index in 0..51 {
            let mut item = step("wide", &format!("s{index:02}"), None, &[], false);
            item.hint.hard_mem_max_bytes = Some(GIB);
            steps.push(item);
        }
        let value = DagConfig {
            steps,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            ..Default::default()
        };

        let (total, chosen) = schedulable_peak_mem_bytes(&value, 51, None);
        assert_eq!(total, 51 * GIB);
        assert_eq!(
            chosen,
            (0..51)
                .map(|index| format!("wide.s{index:02}"))
                .collect::<Vec<_>>()
        );
    }

    // ------------------------------------------- operator build width vs the containment default
    //
    // Reading `CARGO_BUILD_JOBS` back as "operator intent" is NOT the fix: the runner SETS that
    // variable itself, so the in-scope process would read its own scope-wide derivation as an
    // instruction and stop refining downward per step — the 284-wide-against-8-GiB condition.
    // Intent is resolved once, in the outermost process, and forwarded under its own name.

    #[test]
    fn a_positive_integer_is_intent() {
        assert_eq!(parse_build_jobs(Some("12")), Some(12));
        assert_eq!(parse_build_jobs(Some(" 12 ")), Some(12));
    }

    #[test]
    fn nothing_else_is_intent() {
        // Each case named, because "return None for everything" would pass a single-case test
        // and would also throw away the real value above.
        assert_eq!(parse_build_jobs(None), None);
        assert_eq!(parse_build_jobs(Some("")), None);
        assert_eq!(parse_build_jobs(Some("   ")), None);
        assert_eq!(parse_build_jobs(Some("0")), None);
        assert_eq!(parse_build_jobs(Some("-4")), None);
        assert_eq!(parse_build_jobs(Some("8.5")), None);
        assert_eq!(parse_build_jobs(Some("many")), None);
        assert_eq!(parse_build_jobs(Some("8 jobs")), None);
    }

    #[test]
    fn the_awkward_digit_strings_are_not_intent() {
        // THE DIVERGENCE `make cross` CAUGHT IN SPIRIT AND COULD NOT REACH. Python's
        // `str.isdigit()` is not `is_ascii_digit`, and a Python int is not an i64. This table is
        // hand-written here and duplicated verbatim in
        // `py/tests/test_operator_build_width.py::test_the_awkward_digit_strings_answer_exactly_as_the_rust_twin_does`;
        // the two must agree case for case.
        assert_eq!(parse_build_jobs(Some("\u{ff18}")), None); // full-width eight
        assert_eq!(parse_build_jobs(Some("8\u{b2}")), None); // superscript two
        assert_eq!(parse_build_jobs(Some("\u{668}")), None); // Arabic-Indic eight
        assert_eq!(parse_build_jobs(Some("99999999999999999999999")), None);
        assert_eq!(parse_build_jobs(Some("9223372036854775808")), None);
        assert_eq!(parse_build_jobs(Some(&"1".repeat(5000))), None);
        // And the boundaries that must still be honoured, so "reject the awkward ones" cannot
        // become "reject everything large".
        assert_eq!(
            parse_build_jobs(Some("9223372036854775807")),
            Some(9223372036854775807)
        );
        // An i64 parse accepts leading zeros, so Python must too.
        assert_eq!(parse_build_jobs(Some("000000008")), Some(8));
    }

    #[test]
    fn the_outermost_process_reads_the_ambient_variable() {
        assert_eq!(resolve_operator_build_jobs(None, Some("200")), Some(200));
        assert_eq!(resolve_operator_build_jobs(None, None), None);
    }

    #[test]
    fn a_forwarded_answer_beats_the_runners_own_write() {
        // THE DEFECT THIS PREVENTS. In-scope, CARGO_BUILD_JOBS=8 is the runner's own derivation.
        // An empty forwarded value says "already asked; the operator wanted nothing", and that
        // must win over the runner's own number, or per-step refinement stops.
        assert_eq!(resolve_operator_build_jobs(Some(""), Some("8")), None);
        assert_eq!(
            resolve_operator_build_jobs(Some("200"), Some("8")),
            Some(200)
        );
    }

    #[test]
    fn the_containment_default_governs_when_nothing_was_stated() {
        let choice = choose_build_jobs(None, Some(284), Some(8 * GIB));
        assert_eq!(choice.jobs, 8);
        assert_eq!(choice.derived, 8);
        assert_eq!(choice.source(), "containment");
        assert_eq!(choice.jobs, derive_build_jobs(Some(284), Some(8 * GIB)));
    }

    #[test]
    fn a_stated_width_wins_and_the_derivation_is_still_recorded() {
        let choice = choose_build_jobs(Some(200), Some(284), Some(8 * GIB));
        assert_eq!(choice.jobs, 200);
        assert_eq!(
            choice.derived, 8,
            "the number that lost must survive, or an OOM is inexplicable"
        );
        assert_eq!(choice.source(), "operator");
    }

    #[test]
    fn the_notice_names_the_winner_the_loser_and_the_risk() {
        let said = choose_build_jobs(Some(200), Some(284), Some(8 * GIB)).describe();
        assert!(said.contains("honouring CARGO_BUILD_JOBS=200"), "{said}");
        assert!(said.contains("would have chosen 8"), "{said}");
        assert!(said.contains("can still OOM"), "{said}");
    }

    #[test]
    fn the_notice_also_says_when_nothing_was_overridden() {
        // "Told it was overridden, or not overridden" — silence in the second case would leave
        // an operator unable to tell a honoured setting from an ignored one.
        let said = choose_build_jobs(None, Some(284), Some(8 * GIB)).describe();
        assert!(
            said.contains("no CARGO_BUILD_JOBS in the environment"),
            "{said}"
        );
        assert!(said.contains("governs at 8"), "{said}");
        assert!(said.contains("refined downward per step"), "{said}");
    }
}
