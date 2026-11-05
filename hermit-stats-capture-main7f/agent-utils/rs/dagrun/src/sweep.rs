//! Pure planning helpers for graph-wide parallel-scaling sweeps.
//!
//! Execution lives in [`crate::cli`]. Keeping topology discovery, stable DAG ordering, width
//! grids, and user-input parsing here makes the experiment plan inspectable without spawning a
//! command -- important because a target-time sweep deliberately finishes every pass it starts.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap};
use std::path::Path;
use std::time::Duration;

use crate::model::Step;

/// The CPUs available to this process, split into physical cores and logical hardware threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineTopology {
    /// Distinct `(physical_package_id, core_id)` pairs visible through the affinity mask.
    pub physical_cores: i64,
    /// Logical CPUs represented by this topology (normally the process affinity mask).
    pub logical_cpus: i64,
}

impl MachineTopology {
    fn normalized(self) -> Self {
        let logical_cpus = self.logical_cpus.max(1);
        Self {
            physical_cores: self.physical_cores.clamp(1, logical_cpus),
            logical_cpus,
        }
    }
}

/// Parse a Linux CPU-list such as `0-3,8,10-11` into sorted, unique CPU identifiers.
pub fn parse_cpu_list(spec: &str) -> Option<Vec<usize>> {
    let mut cpus = BTreeSet::new();
    if spec.trim().is_empty() {
        return None;
    }
    for raw_part in spec.split(',') {
        let part = raw_part.trim();
        if part.is_empty() {
            return None;
        }
        match part.split_once('-') {
            Some((raw_lo, raw_hi)) => {
                let lo = raw_lo.trim().parse::<usize>().ok()?;
                let hi = raw_hi.trim().parse::<usize>().ok()?;
                if hi < lo {
                    return None;
                }
                cpus.extend(lo..=hi);
            }
            None => {
                cpus.insert(part.parse::<usize>().ok()?);
            }
        }
    }
    (!cpus.is_empty()).then(|| cpus.into_iter().collect())
}

/// Extract this process's allowed CPU identifiers from `/proc/self/status` text.
pub fn affinity_cpus_from_status(status: &str) -> Option<Vec<usize>> {
    affinity_cpus_from_status_bytes(status.as_bytes())
}

fn affinity_cpus_from_status_bytes(status: &[u8]) -> Option<Vec<usize>> {
    let value = crate::procstat::status_field(status, b"Cpus_allowed_list")
        .ok()
        .flatten()?;
    parse_cpu_list(std::str::from_utf8(value).ok()?)
}

/// Derive physical/logical topology for an allowed CPU set.
///
/// `core_identity` returns `(physical_package_id, core_id)` for one logical CPU. If even one
/// allowed CPU lacks usable sysfs topology, physical width conservatively falls back to logical
/// width rather than inventing an SMT ratio from a partial view.
pub fn topology_from_affinity<F>(allowed: &[usize], mut core_identity: F) -> MachineTopology
where
    F: FnMut(usize) -> Option<(i64, i64)>,
{
    if allowed.is_empty() {
        return MachineTopology {
            physical_cores: 1,
            logical_cpus: 1,
        };
    }
    let mut cores = BTreeSet::new();
    for cpu in allowed {
        let Some(identity) = core_identity(*cpu) else {
            return MachineTopology {
                physical_cores: allowed.len() as i64,
                logical_cpus: allowed.len() as i64,
            }
            .normalized();
        };
        cores.insert(identity);
    }
    MachineTopology {
        physical_cores: cores.len() as i64,
        logical_cpus: allowed.len() as i64,
    }
    .normalized()
}

/// Discover process-visible topology from affinity plus Linux sysfs core identities.
pub fn machine_topology() -> MachineTopology {
    let allowed = std::fs::read("/proc/self/status")
        .ok()
        .and_then(|status| affinity_cpus_from_status_bytes(&status))
        .unwrap_or_else(|| {
            let logical = crate::perflog::nproc().max(1) as usize;
            (0..logical).collect()
        });
    topology_from_affinity(&allowed, |cpu| {
        let topology = Path::new("/sys/devices/system/cpu")
            .join(format!("cpu{cpu}"))
            .join("topology");
        let package = std::fs::read_to_string(topology.join("physical_package_id"))
            .ok()?
            .trim()
            .parse::<i64>()
            .ok()?;
        let core = std::fs::read_to_string(topology.join("core_id"))
            .ok()?
            .trim()
            .parse::<i64>()
            .ok()?;
        (package >= 0 && core >= 0).then_some((package, core))
    })
}

/// Cap discovered topology to a tighter effective logical CPU budget (for example `cpu.max`).
pub fn limit_topology(topology: MachineTopology, logical_limit: i64) -> MachineTopology {
    let topology = topology.normalized();
    let logical_cpus = topology.logical_cpus.min(logical_limit.max(1));
    MachineTopology {
        physical_cores: topology.physical_cores.min(logical_cpus),
        logical_cpus,
    }
}

/// Stable topological order, using document order to break ties between ready nodes.
pub fn stable_topological_order(steps: &[Step]) -> Result<Vec<usize>, String> {
    let mut by_tag = HashMap::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        if by_tag.insert(step.tag(), index).is_some() {
            return Err(format!("duplicate step tag '{}'", step.tag()));
        }
    }

    let mut indegree = vec![0usize; steps.len()];
    let mut dependents = vec![Vec::<usize>::new(); steps.len()];
    for (index, step) in steps.iter().enumerate() {
        for dependency in &step.deps {
            let Some(&dependency_index) = by_tag.get(dependency) else {
                return Err(format!(
                    "step '{}' depends on unknown step '{dependency}'",
                    step.tag()
                ));
            };
            indegree[index] += 1;
            dependents[dependency_index].push(index);
        }
    }

    let mut ready = BinaryHeap::new();
    for (index, degree) in indegree.iter().enumerate() {
        if *degree == 0 {
            ready.push(Reverse(index));
        }
    }
    let mut order = Vec::with_capacity(steps.len());
    while let Some(Reverse(index)) = ready.pop() {
        order.push(index);
        for dependent in &dependents[index] {
            indegree[*dependent] -= 1;
            if indegree[*dependent] == 0 {
                ready.push(Reverse(*dependent));
            }
        }
    }
    if order.len() != steps.len() {
        return Err("dependency cycle prevents a topological sweep".to_string());
    }
    Ok(order)
}

/// Stable identity for the guest workload and the channels used to vary its width.
///
/// FNV-1a 64-bit is intentionally simple to reproduce in either implementation. The canonical
/// UTF-8 payload is `tag\0cmd\0cmdtype\0effective_jobs_flag\0effective_jobs_env\0`, followed by
/// the step's sorted environment entries as `key=value\0`.
pub fn workload_digest(step: &Step, default_jobs_flag: &str, default_jobs_env: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    let mut field = |value: &str| {
        for byte in value.as_bytes().iter().copied().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };
    field(&step.tag());
    field(&step.cmd);
    field(step.cmdtype.value());
    field(crate::model::effective_jobs_flag(step, default_jobs_flag));
    field(crate::model::effective_jobs_env(step, default_jobs_env));
    for (key, value) in &step.env {
        field(&format!("{key}={value}"));
    }
    if step.delegated_children {
        field("delegated_children=true");
    }
    format!("{hash:016x}")
}

/// First-pass widths: powers of two through the physical-core count, then exact physical and
/// logical widths. On a 158-core / 316-thread machine this is
/// `1,2,4,8,16,32,64,128,158,316`.
pub fn coarse_widths(topology: MachineTopology) -> Vec<i64> {
    let topology = topology.normalized();
    let mut widths = BTreeSet::new();
    let mut power = 1i64;
    while power < topology.physical_cores {
        widths.insert(power);
        let Some(next) = power.checked_mul(2) else {
            break;
        };
        power = next;
    }
    widths.insert(topology.physical_cores);
    widths.insert(topology.logical_cpus);
    widths.into_iter().collect()
}

/// Return the next cumulative grid by inserting an integer midpoint into every remaining gap.
pub fn refine_width_grid(widths: &[i64]) -> Vec<i64> {
    let sorted: BTreeSet<i64> = widths.iter().copied().filter(|width| *width > 0).collect();
    let values: Vec<i64> = sorted.iter().copied().collect();
    let mut refined = sorted;
    for pair in values.windows(2) {
        let lo = pair[0];
        let hi = pair[1];
        if hi - lo > 1 {
            refined.insert(lo + (hi - lo) / 2);
        }
    }
    refined.into_iter().collect()
}

/// The cumulative grid for a one-based pass number.
pub fn cumulative_width_grid(initial: &[i64], pass: usize) -> Vec<i64> {
    let mut grid: Vec<i64> = initial
        .iter()
        .copied()
        .filter(|width| *width > 0)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for _ in 1..pass.max(1) {
        grid = refine_width_grid(&grid);
    }
    grid
}

/// Parse explicit sweep widths.
///
/// A comma list names exact widths (`1,2,4,8`). A bare `N`
/// means every width in `1..=N`, and `LO..HI` means every width in that inclusive range. Range
/// items may also appear in a comma list.
pub fn parse_widths(raw: &str) -> Result<Vec<i64>, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("invalid --jobs '': expected a positive width or range".to_string());
    }
    if !text.contains(',') {
        let (lo, hi) = parse_inclusive_range(text, !text.contains(".."))?;
        return Ok((lo..=hi).collect());
    }

    let mut widths = BTreeSet::new();
    for raw_item in text.split(',') {
        let item = raw_item.trim();
        if item.is_empty() {
            return Err(format!("invalid --jobs '{raw}': empty comma-list item"));
        }
        if item.contains("..") {
            let (lo, hi) = parse_inclusive_range(item, false)?;
            widths.extend(lo..=hi);
        } else {
            let width = item
                .parse::<i64>()
                .map_err(|_| format!("invalid --jobs '{raw}': '{item}' is not an integer"))?;
            if width < 1 {
                return Err(format!("invalid --jobs '{raw}': widths must be >= 1"));
            }
            widths.insert(width);
        }
    }
    Ok(widths.into_iter().collect())
}

fn parse_inclusive_range(raw: &str, bare_means_one_to_n: bool) -> Result<(i64, i64), String> {
    let (lo, hi) = match raw.split_once("..") {
        Some((raw_lo, raw_hi)) => {
            if raw_hi.contains("..") {
                return Err(format!("invalid --jobs range '{raw}': expected LO..HI"));
            }
            let lo = raw_lo
                .trim()
                .parse::<i64>()
                .map_err(|_| format!("invalid --jobs range '{raw}': not an integer"))?;
            let hi = raw_hi
                .trim()
                .parse::<i64>()
                .map_err(|_| format!("invalid --jobs range '{raw}': not an integer"))?;
            (lo, hi)
        }
        None => {
            let value = raw
                .trim()
                .parse::<i64>()
                .map_err(|_| format!("invalid --jobs '{raw}': not an integer"))?;
            if bare_means_one_to_n {
                (1, value)
            } else {
                (value, value)
            }
        }
    };
    if lo < 1 || hi < lo {
        return Err(format!("invalid --jobs range '{raw}': need 1 <= LO <= HI"));
    }
    Ok((lo, hi))
}

/// Parse a non-negative target duration. No suffix and `s` mean seconds; `ms`, `m`, and `h` scale
/// the value. Fractional values are accepted (`0.5s`, `1.25m`), and zero deliberately still runs
/// the mandatory first pass.
pub fn parse_target_duration(raw: &str) -> Result<Duration, String> {
    let text = raw.trim();
    let lower = text.to_ascii_lowercase();
    let (number, multiplier) = if lower.ends_with("ms") {
        (&text[..text.len() - 2], 0.001)
    } else {
        match lower.chars().last() {
            Some('s') => (&text[..text.len() - 1], 1.0),
            Some('m') => (&text[..text.len() - 1], 60.0),
            Some('h') => (&text[..text.len() - 1], 3600.0),
            _ => (text, 1.0),
        }
    };
    let valid_decimal = !number.is_empty()
        && number.bytes().any(|byte| byte.is_ascii_digit())
        && number
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && number.bytes().filter(|byte| *byte == b'.').count() <= 1;
    if !valid_decimal {
        return Err(format!(
            "invalid --target-time '{raw}': expected seconds or an ms/s/m/h suffix"
        ));
    }
    let value = number.trim().parse::<f64>().map_err(|_| {
        format!("invalid --target-time '{raw}': expected seconds or an ms/s/m/h suffix")
    })?;
    let seconds = value * multiplier;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!(
            "invalid --target-time '{raw}': duration must be finite and >= 0"
        ));
    }
    Duration::try_from_secs_f64(seconds).map_err(|_| {
        format!("invalid --target-time '{raw}': duration is outside the supported range")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ResourceHint, Step};
    use std::collections::BTreeMap;

    fn step(tag: &str, deps: &[&str]) -> Step {
        let (group, job) = tag.split_once('.').unwrap();
        Step {
            group: group.to_string(),
            job: job.to_string(),
            desc: tag.to_string(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".to_string(),
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: deps.iter().map(|dep| dep.to_string()).collect(),
            env: BTreeMap::new(),
            hint: ResourceHint::default(),
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: 0,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            cmdtype: crate::model::CmdType::Unknown,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        }
    }

    #[test]
    fn stable_topology_uses_document_order_for_ready_ties() {
        let steps = vec![
            step("g.last", &["g.left", "g.right"]),
            step("g.right", &["g.root"]),
            step("g.root", &[]),
            step("g.left", &["g.root"]),
            step("g.side", &[]),
        ];
        let order = stable_topological_order(&steps).unwrap();
        let tags: Vec<String> = order.iter().map(|index| steps[*index].tag()).collect();
        assert_eq!(tags, ["g.root", "g.right", "g.left", "g.last", "g.side"]);
    }

    #[test]
    fn affinity_topology_counts_packages_and_cores() {
        let topology = topology_from_affinity(&[0, 1, 2, 3], |cpu| Some((0, (cpu / 2) as i64)));
        assert_eq!(
            topology,
            MachineTopology {
                physical_cores: 2,
                logical_cpus: 4
            }
        );
        assert_eq!(
            affinity_cpus_from_status("Name:\tx\nCpus_allowed_list:\t0-2,7\n"),
            Some(vec![0, 1, 2, 7])
        );
        assert_eq!(
            affinity_cpus_from_status(
                "Name:\tx\nCpus_allowed_list:\t99\nState:\tS\nCpus_allowed_list:\t0-2,7\n"
            ),
            None
        );
    }

    #[test]
    fn coarse_and_cumulative_grids_match_the_large_smt_host_policy() {
        let first = coarse_widths(MachineTopology {
            physical_cores: 158,
            logical_cpus: 316,
        });
        assert_eq!(first, [1, 2, 4, 8, 16, 32, 64, 128, 158, 316]);
        let second = cumulative_width_grid(&first, 2);
        assert_eq!(
            second,
            [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 143, 158, 237, 316]
        );
        assert_eq!(cumulative_width_grid(&first, 1), first);
        assert!(cumulative_width_grid(&first, 3).len() > second.len());
        assert_eq!(
            limit_topology(
                MachineTopology {
                    physical_cores: 158,
                    logical_cpus: 316,
                },
                64,
            ),
            MachineTopology {
                physical_cores: 64,
                logical_cpus: 64,
            }
        );
    }

    #[test]
    fn explicit_widths_preserve_legacy_ranges_and_add_lists() {
        assert_eq!(parse_widths("4").unwrap(), [1, 2, 3, 4]);
        assert_eq!(parse_widths("2..4").unwrap(), [2, 3, 4]);
        assert_eq!(parse_widths("8,1,4,4").unwrap(), [1, 4, 8]);
        assert_eq!(parse_widths("1,3..5,8").unwrap(), [1, 3, 4, 5, 8]);
        assert!(parse_widths("1,0").is_err());
    }

    #[test]
    fn target_duration_accepts_zero_and_suffixes() {
        assert_eq!(parse_target_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_target_duration("0.5s").unwrap().as_millis(), 500);
        assert_eq!(parse_target_duration("250MS").unwrap().as_millis(), 250);
        assert_eq!(parse_target_duration("1.5m").unwrap().as_secs(), 90);
        assert_eq!(parse_target_duration("2h").unwrap().as_secs(), 7200);
        assert!(parse_target_duration("-1s").is_err());
        assert!(parse_target_duration("NaN").is_err());
        assert!(parse_target_duration("1e3").is_err());
        assert!(parse_target_duration("+1").is_err());
    }

    #[test]
    fn workload_digest_covers_width_channels_and_sorted_environment() {
        let mut base = step("g.j", &[]);
        base.cmd = "echo hi".to_string();
        base.jobs_flag = Some("--workers=".to_string());
        base.jobs_env = Some("N".to_string());
        base.env.insert("B".to_string(), "2".to_string());
        base.env.insert("A".to_string(), "1".to_string());
        let digest = workload_digest(&base, "-j", "");
        assert_eq!(digest, "424d54398eaf4baa");
        assert_eq!(digest, workload_digest(&base, "ignored", "IGNORED"));

        let mut changed = base.clone();
        changed.cmd.push('!');
        assert_ne!(digest, workload_digest(&changed, "-j", ""));

        let mut typed = base.clone();
        typed.cmdtype = crate::model::CmdType::GenericWithFlag;
        assert_ne!(digest, workload_digest(&typed, "-j", ""));
    }
}
