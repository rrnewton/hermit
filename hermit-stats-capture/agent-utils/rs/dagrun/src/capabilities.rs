//! The enforcement manifest, derived from the guards that implement it.
//!
//! The `capabilities` subcommand advertises which containment guards this engine really
//! applies. That manifest used to be a hand-typed JSON string literal, which meant it could claim
//! enforcement that no longer happened: nothing tied the literal to a guard site, so deleting or
//! short-circuiting a guard left the advertisement intact. The py-vs-rs differential compares the
//! two manifests byte-for-byte, which stops the two ENGINES from drifting apart but cannot notice
//! both being wrong together, nor a manifest drifting from the guard sites inside its own engine.
//!
//! So the manifest is generated here instead. [`ENFORCEMENT_REGISTRY`] is the single declaration;
//! [`enforcement_manifest`] serializes it key-sorted, compact and with lowercase booleans, so the
//! two editions agree BY CONSTRUCTION rather than because two people kept two literals in step;
//! and [`is_enforced`] is consulted by the contractual guard. The uncontained `cpu_timeout`
//! fallback is the explicit exception: it may attempt a lower-bound intervention while the
//! published contract remains false because it cannot provide cgroup-equivalent coverage.
//!
//! THE MANIFEST IS PER LANE, BECAUSE ENFORCEMENT IS. Most of these guards are implemented by
//! reading or writing a cgroup, and a run that could not get one (`--allow-cgroup-failure`,
//! `--unsafe-no-cgroups`, or a library call with no manager) does not have them — the step still
//! runs, still exits 0, and is still reported green. A single flat column could only ever describe
//! one of those two worlds, and it described the boxed one, so an uncontained run was advertised
//! guards it was not getting: a step declaring `cpu_timeout: 3` could burn 60 CPU-seconds and be
//! reported green while the manifest said the budget held. Each [`Capability`] therefore carries
//! TWO flags, `contained` and `uncontained`, and [`is_enforced`] takes the [`Lane`] the run is
//! actually on. The lane is not a comment about the guard site; it is the argument the guard site
//! passes. The best-effort uncontained CPU fallback is deliberately outside that Boolean guarantee.
//!
//! [`is_enforced`] panics on an unknown key. A guard site that misspells its capability therefore
//! fails loudly at the moment it runs, rather than reading as "not enforced" and silently
//! switching the guard off — which is the exact failure this module exists to prevent.
//!
//! WHICH KEYS ACTUALLY GATE SOMETHING. Four of the nine are directly load-bearing on both lanes.
//! `cpu_timeout` is load-bearing for exact contained enforcement while an explicitly
//! non-contractual procfs fallback may still act on the uncontained lane:
//!
//! | key             | guard site that consults `is_enforced`                              |
//! |-----------------|---------------------------------------------------------------------|
//! | `cpu_timeout`   | exact cgroup `cpu.stat`; uncontained fallback is non-contractual       |
//! | `wall_timeout`  | the scheduler's per-step wait deadline                               |
//! | `oom_detection` | the post-step `memory.events` read: any positive `oom`, `oom_kill` or `oom_group_kill` |
//! | `memory_max`    | the per-step inner `memory.max` write in the cgroup manager          |
//!
//! `pids_guard` gates the per-step `pids.max` write in the companion reference engine, which is
//! the only one with any pids plumbing; this engine has none, which is what `pids_guard: false`
//! says on both lanes. The remaining four — `cpu_affinity`, `cpu_bandwidth`, `run_timeout` and `write_domains`
//! — are declarations only: the registry records them and the manifest publishes them, but no
//! code consults their flag, so flipping one changes the advertisement WITHOUT changing
//! behaviour. Wiring those is a further step, and saying so here is better than implying a
//! coverage that does not exist.

use std::sync::{Mutex, RwLock};

/// Which containment world a run is in; the manifest publishes one column per lane.
///
/// `Contained` is the boxed lane: a cgroup-v2 child per step, so the cgroup reads and writes the
/// guards are made of actually happen. `Uncontained` is what `--allow-cgroup-failure`,
/// `--unsafe-no-cgroups` or a library call with no manager gives you: the step still runs, but
/// every cgroup-backed guard is absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Boxed: a per-step cgroup exists, so cgroup-backed guards can act.
    Contained,
    /// Unboxed: no cgroup, so every cgroup-backed guard is simply not there.
    Uncontained,
}

impl Lane {
    /// The manifest key this lane is published under.
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Contained => "contained",
            Lane::Uncontained => "uncontained",
        }
    }

    /// The lane a run is on, given whether cgroup boxing is in force.
    pub fn of_boxed(boxed: bool) -> Self {
        if boxed {
            Lane::Contained
        } else {
            Lane::Uncontained
        }
    }
}

/// One advertised containment guard, on both lanes.
///
/// `key` is the manifest key (and what a guard site passes to [`is_enforced`]); `contained` and
/// `uncontained` are whether this engine really applies it with and without cgroup boxing;
/// `summary` is the human sentence that used to live in the comment above the string literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    /// Manifest key; also what a guard site passes to [`is_enforced`].
    pub key: &'static str,
    /// Whether this engine really applies the guard under cgroup boxing.
    pub contained: bool,
    /// Whether this engine really applies the guard with boxing off.
    pub uncontained: bool,
    /// Human sentence describing what the guard does.
    pub summary: &'static str,
}

impl Capability {
    /// This capability's flag for `lane`.
    pub fn enforced_on(&self, lane: Lane) -> bool {
        match lane {
            Lane::Contained => self.contained,
            Lane::Uncontained => self.uncontained,
        }
    }
}

/// Every enforcement guard this engine advertises, and whether it is real ON EACH LANE. This is
/// the ONLY place the answer is written down: the manifest is generated from it and the guard
/// sites listed in the module docs consult it. The boxed smoke tests in each build anchor the
/// `contained` column to real behaviour wherever a cgroup-v2 + systemd --user scope exists; in the
/// `uncontained` column only `run_timeout`, `wall_timeout` and `write_domains` survive, because
/// those are guaranteed. A best-effort procfs CPU floor may also act, but remains outside
/// the Boolean contract because it misses process-group escapees.
pub const ENFORCEMENT_REGISTRY: &[Capability] = &[
    Capability {
        key: "cpu_affinity",
        contained: true,
        // `--cores` REFUSES rather than degrading, so the guard is not in force here: it is
        // not that a weaker version runs, it is that the run does not start.
        uncontained: false,
        summary: "opt-in --cores K: constrain the WHOLE run tree to K least-busy free cores with \
                  an exact, verified cgroup cpuset; refuse when unavailable",
    },
    Capability {
        key: "cpu_bandwidth",
        contained: true,
        uncontained: false,
        summary: "boxed run: exact outer cpu.max = --max-cpus x period, read back before \
                  execution",
    },
    Capability {
        key: "cpu_timeout",
        contained: true,
        // THE DEFECT #75 NAMES: no cgroup, no cpu.stat, no CPU-time enforcement at all.
        uncontained: false,
        summary: "per-step user+system CPU budget (cgroup cpu.stat), reaped over budget",
    },
    Capability {
        key: "memory_max",
        contained: true,
        uncontained: false,
        summary: "per-step inner memory.max cap (kernel OOM-kills the step at its cap)",
    },
    Capability {
        key: "oom_detection",
        contained: true,
        uncontained: false,
        summary: "failure attributed to OOM when any cgroup memory.events oom, oom_kill or \
                  oom_group_kill count is positive",
    },
    Capability {
        key: "pids_guard",
        contained: false,
        uncontained: false,
        summary: "per-step PID/thread ceiling: the reference engine's cgroup manager can write \
                  pids.max, but no caller sets the limit and this engine has no pids plumbing at \
                  all, so the write is gated off and nothing applies a PID ceiling",
    },
    Capability {
        key: "run_timeout",
        contained: true,
        uncontained: true,
        summary: "OUTER wall budget for the WHOLE run: the scheduler cuts in-flight steps and \
                  still reports (works boxed or unboxed); under boxing it is additionally backed \
                  by the scope's systemd RuntimeMaxSec, set strictly later so the reporting bound \
                  fires first",
    },
    Capability {
        key: "wall_timeout",
        contained: true,
        uncontained: true,
        summary: "per-step wall-clock ceiling (load-dependent; active with or without boxing)",
    },
    Capability {
        key: "write_domains",
        contained: true,
        uncontained: true,
        summary: "pre-execution closed-vocabulary declaration guard; omission/unknown/duplicate \
                  domains refuse before any node starts when the DAG opts in",
    },
];

/// Bracket support only (see [`with_registry_override`]); `None` means "use the real one".
static OVERRIDE: RwLock<Option<Vec<Capability>>> = RwLock::new(None);
/// Serializes brackets against each other AND against anything that reads the unbracketed
/// registry: the override is process-wide, and a test harness runs test functions on many
/// threads, so without this a bracket's window is visible to an unrelated concurrent reader.
/// [`with_registry_override`] and [`with_registry_pinned`] are the two sides of that agreement.
static OVERRIDE_SERIAL: Mutex<()> = Mutex::new(());

fn with_active<T>(f: impl FnOnce(&[Capability]) -> T) -> T {
    let guard = OVERRIDE.read().unwrap_or_else(|e| e.into_inner());
    match guard.as_deref() {
        Some(active) => f(active),
        None => f(ENFORCEMENT_REGISTRY),
    }
}

/// The machine-readable manifest: two lanes, key-sorted, compact, lowercase booleans.
///
/// Byte-identical to the companion reference engine's manifest by construction — both
/// serialize the same shape from their own registry, so a guard present in one build and missing
/// from the other shows up as a differing manifest in `cross`.
///
/// Both lanes carry the same sorted key set, so a reader can diff the two columns: a key present
/// in one and absent from the other would read as "not applicable" when it means "nobody wrote it
/// down".
pub fn enforcement_manifest() -> String {
    with_active(|active| {
        let mut keys: Vec<&Capability> = active.iter().collect();
        keys.sort_unstable_by_key(|c| c.key);
        let lane_body = |lane: Lane| {
            keys.iter()
                .map(|c| format!("\"{}\":{}", c.key, c.enforced_on(lane)))
                .collect::<Vec<_>>()
                .join(",")
        };
        // Lane order is the sorted key order of the outer object too ("contained" <
        // "uncontained"), which is what the reference engine's `sort_keys=True` emits.
        format!(
            "{{\"{}\":{{{}}},\"{}\":{{{}}}}}",
            Lane::Contained.as_str(),
            lane_body(Lane::Contained),
            Lane::Uncontained.as_str(),
            lane_body(Lane::Uncontained),
        )
    })
}

/// Whether this engine really applies the guard named `key` on `lane`.
///
/// Call this AT the guard site, not near it, and pass the lane the RUN is on rather than the lane
/// the guard was written for, so the flag and the behaviour cannot part company on either lane.
///
/// # Panics
///
/// For an unknown key. A misspelled capability at a guard site is a bug that must be loud,
/// because the quiet reading of it — "unknown, so not enforced" — would silently disable the very
/// guard the caller was writing.
pub fn is_enforced(key: &str, lane: Lane) -> bool {
    with_active(|active| match active.iter().find(|c| c.key == key) {
        Some(c) => c.enforced_on(lane),
        None => {
            let mut known: Vec<&str> = active.iter().map(|c| c.key).collect();
            known.sort_unstable();
            panic!(
                "unknown enforcement capability {key:?}; a guard site must name a capability \
                 declared in ENFORCEMENT_REGISTRY (known: {})",
                known.join(", ")
            )
        }
    })
}

/// Temporarily flip one capability's flag ON BOTH LANES for the duration of `f`. **Brackets
/// only.**
///
/// Both lanes move so that the guard site is off whichever lane the bracket's manager puts it on;
/// [`with_lane_registry_override`] moves exactly one column, which is how a bracket proves the
/// lane argument is honoured rather than ignored.
///
/// This exists so a test can prove the coupling this module claims: flip one flag and assert that
/// BOTH the published manifest and the guarded behaviour move. A test that only re-derived the
/// manifest from a registry it had just built itself would be tautological; a test that only
/// checked behaviour would not notice the manifest lying.
///
/// Mirrored by the companion reference engine's `registry_override` context manager.
///
/// # Panics
///
/// For an unknown key, so a stale bracket cannot silently flip nothing.
pub fn with_registry_override<T>(key: &str, enforced: bool, f: impl FnOnce() -> T) -> T {
    with_override(key, enforced, None, f)
}

/// As [`with_registry_override`], but flips only `lane`'s column. **Brackets only.**
///
/// # Panics
///
/// For an unknown key, so a stale bracket cannot silently flip nothing.
pub fn with_lane_registry_override<T>(
    key: &str,
    enforced: bool,
    lane: Lane,
    f: impl FnOnce() -> T,
) -> T {
    with_override(key, enforced, Some(lane), f)
}

fn with_override<T>(key: &str, enforced: bool, lane: Option<Lane>, f: impl FnOnce() -> T) -> T {
    assert!(
        ENFORCEMENT_REGISTRY.iter().any(|c| c.key == key),
        "unknown enforcement capability {key:?}"
    );
    let _serial = OVERRIDE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let flipped: Vec<Capability> = ENFORCEMENT_REGISTRY
        .iter()
        .map(|c| {
            if c.key == key {
                Capability {
                    contained: if matches!(lane, None | Some(Lane::Contained)) {
                        enforced
                    } else {
                        c.contained
                    },
                    uncontained: if matches!(lane, None | Some(Lane::Uncontained)) {
                        enforced
                    } else {
                        c.uncontained
                    },
                    ..*c
                }
            } else {
                *c
            }
        })
        .collect();
    // Restore on the way out even if `f` panics, so one failing bracket cannot leave a
    // process-wide registry flipped under every test that follows it.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            *OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
    *OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = Some(flipped);
    let _restore = Restore;
    f()
}

/// Run `f` with the real registry held steady against a concurrent [`with_registry_override`].
/// **Brackets only.**
///
/// A bracket's flip is process-wide, so a test asserting what the UNBRACKETED registry says has to
/// take the same turn as the brackets do, or it can read another test's window and fail for a
/// reason that has nothing to do with it.
pub fn with_registry_pinned<T>(f: impl FnOnce() -> T) -> T {
    let _serial = OVERRIDE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest is written out LITERALLY here, not re-derived from the registry: a test that
    /// serialized the registry and compared it to the registry would pass no matter what the
    /// registry said. This is the byte string the `capabilities` subcommand promises, and the
    /// companion reference engine must print the same one.
    const PUBLISHED_MANIFEST: &str = concat!(
        r#"{"contained":{"cpu_affinity":true,"cpu_bandwidth":true,"cpu_timeout":true,"#,
        r#""memory_max":true,"oom_detection":true,"pids_guard":false,"run_timeout":true,"#,
        r#""wall_timeout":true,"write_domains":true},"#,
        r#""uncontained":{"cpu_affinity":false,"cpu_bandwidth":false,"cpu_timeout":false,"#,
        r#""memory_max":false,"oom_detection":false,"pids_guard":false,"run_timeout":true,"#,
        r#""wall_timeout":true,"write_domains":true}}"#,
    );

    #[test]
    fn manifest_is_exactly_the_published_bytes() {
        let manifest = with_registry_pinned(enforcement_manifest);
        assert_eq!(manifest, PUBLISHED_MANIFEST);
    }

    #[test]
    fn manifest_moves_when_a_flag_is_flipped() {
        let flipped = with_registry_override("memory_max", false, enforcement_manifest);
        assert!(flipped.contains(r#""memory_max":false"#), "{flipped}");
        let restored = with_registry_pinned(enforcement_manifest);
        assert!(restored.contains(r#""memory_max":true"#), "{restored}");
    }

    /// The lane argument has to reach the answer, or every guard site is silently asking about
    /// the boxed world. Values are named literally rather than read back out of the registry.
    #[test]
    fn is_enforced_answers_per_lane() {
        with_registry_pinned(|| {
            assert!(is_enforced("cpu_timeout", Lane::Contained));
            assert!(!is_enforced("cpu_timeout", Lane::Uncontained));
            assert!(is_enforced("memory_max", Lane::Contained));
            assert!(!is_enforced("memory_max", Lane::Uncontained));
            // Scheduler-side bounds need no cgroup, so they hold on both lanes.
            assert!(is_enforced("wall_timeout", Lane::Contained));
            assert!(is_enforced("wall_timeout", Lane::Uncontained));
            assert!(is_enforced("run_timeout", Lane::Uncontained));
            assert!(is_enforced("write_domains", Lane::Uncontained));
            // Enforced on neither.
            assert!(!is_enforced("pids_guard", Lane::Contained));
            assert!(!is_enforced("pids_guard", Lane::Uncontained));
        });
    }

    /// `Lane::of_boxed` is what every guard site uses to name its lane, so pin the mapping
    /// rather than trusting the two-arm match to stay the right way round.
    #[test]
    fn of_boxed_maps_boxing_to_the_lane_the_manifest_publishes() {
        assert_eq!(Lane::of_boxed(true), Lane::Contained);
        assert_eq!(Lane::of_boxed(false), Lane::Uncontained);
        assert_eq!(Lane::Contained.as_str(), "contained");
        assert_eq!(Lane::Uncontained.as_str(), "uncontained");
    }

    #[test]
    fn is_enforced_reads_the_registry_and_the_override() {
        assert!(is_enforced("cpu_timeout", Lane::Contained));
        assert!(!is_enforced("pids_guard", Lane::Contained));
        assert!(!with_registry_override("cpu_timeout", false, || {
            is_enforced("cpu_timeout", Lane::Contained)
        }));
        assert!(is_enforced("cpu_timeout", Lane::Contained));
    }

    /// A single-lane bracket must move ONE column. If it moved both, a bracket claiming to
    /// prove the lane argument matters would prove nothing.
    #[test]
    fn a_single_lane_override_leaves_the_other_lane_alone() {
        with_lane_registry_override("wall_timeout", false, Lane::Uncontained, || {
            assert!(is_enforced("wall_timeout", Lane::Contained));
            assert!(!is_enforced("wall_timeout", Lane::Uncontained));
            let manifest = enforcement_manifest();
            assert!(manifest.contains(r#""wall_timeout":true"#), "{manifest}");
            assert!(manifest.contains(r#""wall_timeout":false"#), "{manifest}");
        });
        with_registry_pinned(|| assert_eq!(enforcement_manifest(), PUBLISHED_MANIFEST));
    }

    #[test]
    #[should_panic(expected = "unknown enforcement capability \"cpu_timout\"")]
    fn a_misspelled_guard_site_panics_instead_of_reading_as_unenforced() {
        with_registry_pinned(|| is_enforced("cpu_timout", Lane::Contained));
    }

    #[test]
    fn a_panicking_bracket_still_restores_the_registry() {
        let caught = std::panic::catch_unwind(|| {
            with_registry_override("memory_max", false, || panic!("bracket body blew up"));
        });
        assert!(caught.is_err());
        with_registry_pinned(|| assert!(is_enforced("memory_max", Lane::Contained)));
    }
}
