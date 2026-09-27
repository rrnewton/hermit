/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Machine-readable evidence that the selected backend performed its own work.

use serde::Deserialize;
use serde::Serialize;

/// The backend-specific value used by compatibility-envelope scoring.
///
/// The variants keep each number attached to what it counts. A bare numeric
/// field would allow a scheduler-turn count to be consumed as a mapped-site or
/// branch count while remaining perfectly well formed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
pub enum BackendEngagement {
    Ptrace {
        scheduler_turns: u64,
    },
    E9patch {
        candidate_sites: u64,
        mapped_sites: u64,
        b0_sites: u64,
    },
    Dbt {
        counted_branches: u64,
    },
    /// LiteInst engagement measured over one run (schema 3).
    ///
    /// Both fractions share one denominator, `syscall_entries`, and repeat it so
    /// that neither can be read without the population it was measured over.
    /// `engagement_class` is derived from the numerators and checked by
    /// [`BackendEngagementReport::validate`]; a record whose patched-site count
    /// is zero is always `control` and is never evidence of LiteInst parity.
    Liteinst {
        /// Which LiteInst mechanism ran. Only `host_hybrid` exists today.
        mode: LiteinstMode,
        /// `control` when no syscall entry went through a patched site.
        engagement_class: LiteinstEngagementClass,
        /// Every guest syscall entry handled by the Detcore Tool, on every
        /// thread, including the entries before the first `execve` completes.
        /// This is `RunSummary::syscalls` of the same run.
        syscall_entries: u64,
        /// M2: syscall entries that reached the Tool through an installed
        /// LiteInst patch instead of an ordinary seccomp stop.
        patched_site_entries: EngagementFraction,
        /// M3: syscall entries that never caused a ptrace stop.
        ptrace_free_entries: EngagementFraction,
        /// First seccomp stop at each previously unseen site after the runtime
        /// handshake; each one is a syscall entry and a patch attempt.
        first_site_seccomp: u64,
        /// Patch attempts that installed a hook.
        ptrace_installations: u64,
        /// Syscall entries at sites whose patch was refused, including the
        /// first entry, which `first_site_seccomp` also counts.
        fallback_entries: u64,
        /// Distinct (process, exec generation, rip) sites patched.
        patched_sites: u64,
        /// Distinct (process, exec generation, rip) sites attempted.
        candidate_sites: u64,
    },
}

/// A count and the population it was measured over.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EngagementFraction {
    pub numerator: u64,
    pub denominator: u64,
}

/// The LiteInst mechanism that produced a record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiteinstMode {
    /// The LD_PRELOAD runtime installs patches, and every installed hook
    /// returns to the ptrace host through a SIGTRAP. No entry avoids ptrace.
    HostHybrid,
}

/// How far a LiteInst run engaged its own mechanism.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiteinstEngagementClass {
    /// No syscall entry went through a patched site. The run exercised the
    /// ptrace path only, so its parity is ptrace parity, not LiteInst parity.
    Control,
    /// Some entries went through patched sites, but every one still stopped
    /// in ptrace.
    PatchedViaPtrace,
    /// Some entries completed without any ptrace stop.
    PtraceFree,
}

impl LiteinstEngagementClass {
    /// The only class consistent with the two numerators.
    pub const fn derive(patched_site_entries: u64, ptrace_free_entries: u64) -> Self {
        if patched_site_entries == 0 {
            Self::Control
        } else if ptrace_free_entries == 0 {
            Self::PatchedViaPtrace
        } else {
            Self::PtraceFree
        }
    }

    /// Whether a parity result from this run may be counted as LiteInst parity.
    pub const fn counts_as_liteinst_parity(self) -> bool {
        !matches!(self, Self::Control)
    }
}

/// Host-hybrid LiteInst counters from one run, before they are joined with the
/// run's Tool-side syscall count.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LiteinstHostHybridCounters {
    /// Patched-site entries serviced through the ptrace-host SIGTRAP path.
    pub direct_hooks: u64,
    pub first_site_seccomp: u64,
    pub ptrace_installations: u64,
    pub fallback_entries: u64,
    pub patched_sites: u64,
    pub candidate_sites: u64,
}

impl BackendEngagement {
    /// Join host-hybrid LiteInst counters with the same run's Tool syscall count.
    pub fn liteinst_host_hybrid(
        syscall_entries: u64,
        counters: LiteinstHostHybridCounters,
    ) -> Result<Self, String> {
        let patched = counters.direct_hooks;
        // The host hybrid returns every installed hook through ptrace.
        let ptrace_free = 0;
        let engagement = Self::Liteinst {
            mode: LiteinstMode::HostHybrid,
            engagement_class: LiteinstEngagementClass::derive(patched, ptrace_free),
            syscall_entries,
            patched_site_entries: EngagementFraction {
                numerator: patched,
                denominator: syscall_entries,
            },
            ptrace_free_entries: EngagementFraction {
                numerator: ptrace_free,
                denominator: syscall_entries,
            },
            first_site_seccomp: counters.first_site_seccomp,
            ptrace_installations: counters.ptrace_installations,
            fallback_entries: counters.fallback_entries,
            patched_sites: counters.patched_sites,
            candidate_sites: counters.candidate_sites,
        };
        engagement.validate_counts()?;
        Ok(engagement)
    }

    fn validate_counts(&self) -> Result<(), String> {
        let Self::Liteinst {
            mode,
            engagement_class,
            syscall_entries,
            patched_site_entries,
            ptrace_free_entries,
            first_site_seccomp,
            ptrace_installations,
            fallback_entries: _,
            patched_sites,
            candidate_sites,
        } = self
        else {
            return Ok(());
        };
        for (name, fraction) in [
            ("patched_site_entries", patched_site_entries),
            ("ptrace_free_entries", ptrace_free_entries),
        ] {
            if fraction.denominator != *syscall_entries {
                return Err(format!(
                    "liteinst {name} denominator {} is not syscall_entries {syscall_entries}",
                    fraction.denominator
                ));
            }
            if fraction.numerator > fraction.denominator {
                return Err(format!(
                    "liteinst {name} numerator {} exceeds its denominator {}",
                    fraction.numerator, fraction.denominator
                ));
            }
        }
        if *mode == LiteinstMode::HostHybrid && ptrace_free_entries.numerator != 0 {
            return Err(format!(
                "liteinst host_hybrid cannot report {} ptrace-free entries",
                ptrace_free_entries.numerator
            ));
        }
        // A first-site entry is serviced by an ordinary seccomp stop and a
        // patched-site entry by a hook, so the two are disjoint Tool entries.
        // This cross-checks the Reverie host counters against Detcore's count.
        let host_entries = patched_site_entries
            .numerator
            .checked_add(*first_site_seccomp)
            .ok_or("liteinst entry counters overflow")?;
        if host_entries > *syscall_entries {
            return Err(format!(
                "liteinst patched-site ({}) plus first-site ({first_site_seccomp}) entries exceed \
                 syscall_entries {syscall_entries}",
                patched_site_entries.numerator
            ));
        }
        if ptrace_installations > first_site_seccomp {
            return Err(format!(
                "liteinst ptrace_installations {ptrace_installations} exceed first_site_seccomp \
                 {first_site_seccomp}"
            ));
        }
        if patched_sites > candidate_sites {
            return Err(format!(
                "liteinst patched_sites {patched_sites} exceed candidate_sites {candidate_sites}"
            ));
        }
        let derived = LiteinstEngagementClass::derive(
            patched_site_entries.numerator,
            ptrace_free_entries.numerator,
        );
        if *engagement_class != derived {
            return Err(format!(
                "liteinst engagement_class {engagement_class:?} contradicts its counters, which \
                 derive {derived:?}"
            ));
        }
        Ok(())
    }
}

/// One complete `--backend-engagement-json` record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackendEngagementReport {
    pub schema: u8,
    pub engagement: BackendEngagement,
}

impl BackendEngagementReport {
    /// Schema written by this producer. Schema 3 added `Liteinst`.
    pub const SCHEMA: u8 = 3;

    /// Oldest schema whose records are still accepted. Schema 2 records carry
    /// only the ptrace, e9patch, and DBT variants, whose shape is unchanged.
    pub const OLDEST_READABLE_SCHEMA: u8 = 2;

    pub const fn new(engagement: BackendEngagement) -> Self {
        Self {
            schema: Self::SCHEMA,
            engagement,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(Self::OLDEST_READABLE_SCHEMA..=Self::SCHEMA).contains(&self.schema) {
            return Err(format!(
                "backend-engagement schema must be between {} and {}, got {}",
                Self::OLDEST_READABLE_SCHEMA,
                Self::SCHEMA,
                self.schema
            ));
        }
        if matches!(self.engagement, BackendEngagement::Liteinst { .. }) && self.schema < 3 {
            return Err(format!(
                "a liteinst backend-engagement record requires schema 3, got {}",
                self.schema
            ));
        }
        self.engagement.validate_counts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_and_counter_cannot_be_recombined() {
        let report = BackendEngagementReport::new(BackendEngagement::E9patch {
            candidate_sites: 7,
            mapped_sites: 7,
            b0_sites: 0,
        });
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(
            json,
            r#"{"schema":3,"engagement":{"backend":"e9patch","candidate_sites":7,"mapped_sites":7,"b0_sites":0}}"#
        );
        let changed =
            serde_json::to_string(&BackendEngagementReport::new(BackendEngagement::E9patch {
                candidate_sites: 8,
                mapped_sites: 8,
                b0_sites: 0,
            }))
            .unwrap();
        assert_ne!(json, changed, "the producer's value must reach the record");

        let mismatched = r#"{"schema":2,"engagement":{"backend":"e9patch","counted_branches":7}}"#;
        let error = serde_json::from_str::<BackendEngagementReport>(mismatched).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown field `counted_branches`")
        );
    }

    #[test]
    fn unsupported_schema_refuses_by_name() {
        for schema in [1, 4] {
            let report = BackendEngagementReport {
                schema,
                engagement: BackendEngagement::Dbt {
                    counted_branches: 11,
                },
            };
            assert_eq!(
                report.validate().unwrap_err(),
                format!("backend-engagement schema must be between 2 and 3, got {schema}")
            );
        }
    }

    #[test]
    fn schema_2_records_remain_readable() {
        let json = r#"{"schema":2,"engagement":{"backend":"ptrace","scheduler_turns":5}}"#;
        let report: BackendEngagementReport = serde_json::from_str(json).unwrap();
        report.validate().unwrap();
        assert_eq!(
            report.engagement,
            BackendEngagement::Ptrace { scheduler_turns: 5 }
        );
    }

    fn counters(direct_hooks: u64) -> LiteinstHostHybridCounters {
        LiteinstHostHybridCounters {
            direct_hooks,
            first_site_seccomp: 4,
            ptrace_installations: 3,
            fallback_entries: 2,
            patched_sites: 3,
            candidate_sites: 4,
        }
    }

    #[test]
    fn liteinst_without_patched_site_entries_is_a_control() {
        let engagement = BackendEngagement::liteinst_host_hybrid(400, counters(0)).unwrap();
        let BackendEngagement::Liteinst {
            engagement_class, ..
        } = &engagement
        else {
            panic!("expected a liteinst record");
        };
        assert_eq!(*engagement_class, LiteinstEngagementClass::Control);
        assert!(!engagement_class.counts_as_liteinst_parity());
        let json = serde_json::to_string(&BackendEngagementReport::new(engagement)).unwrap();
        assert_eq!(
            json,
            r#"{"schema":3,"engagement":{"backend":"liteinst","mode":"host_hybrid","engagement_class":"control","syscall_entries":400,"patched_site_entries":{"numerator":0,"denominator":400},"ptrace_free_entries":{"numerator":0,"denominator":400},"first_site_seccomp":4,"ptrace_installations":3,"fallback_entries":2,"patched_sites":3,"candidate_sites":4}}"#
        );
    }

    #[test]
    fn liteinst_patched_site_entries_are_engaged_but_not_ptrace_free() {
        let engagement = BackendEngagement::liteinst_host_hybrid(400, counters(90)).unwrap();
        let BackendEngagement::Liteinst {
            engagement_class,
            patched_site_entries,
            ptrace_free_entries,
            ..
        } = &engagement
        else {
            panic!("expected a liteinst record");
        };
        assert_eq!(*engagement_class, LiteinstEngagementClass::PatchedViaPtrace);
        assert!(engagement_class.counts_as_liteinst_parity());
        assert_eq!(
            *patched_site_entries,
            EngagementFraction {
                numerator: 90,
                denominator: 400
            }
        );
        assert_eq!(ptrace_free_entries.numerator, 0);
        BackendEngagementReport::new(engagement).validate().unwrap();
    }

    fn liteinst_json(schema: u8, class: &str, patched: u64, denominator: u64) -> String {
        format!(
            r#"{{"schema":{schema},"engagement":{{"backend":"liteinst","mode":"host_hybrid","engagement_class":"{class}","syscall_entries":400,"patched_site_entries":{{"numerator":{patched},"denominator":{denominator}}},"ptrace_free_entries":{{"numerator":0,"denominator":400}},"first_site_seccomp":4,"ptrace_installations":3,"fallback_entries":2,"patched_sites":3,"candidate_sites":4}}}}"#
        )
    }

    #[test]
    fn a_control_cannot_be_relabelled_as_engaged() {
        let forged: BackendEngagementReport =
            serde_json::from_str(&liteinst_json(3, "patched_via_ptrace", 0, 400)).unwrap();
        assert_eq!(
            forged.validate().unwrap_err(),
            "liteinst engagement_class PatchedViaPtrace contradicts its counters, which derive \
             Control"
        );
        let honest: BackendEngagementReport =
            serde_json::from_str(&liteinst_json(3, "control", 0, 400)).unwrap();
        honest.validate().unwrap();
    }

    #[test]
    fn liteinst_record_needs_the_class_the_denominator_and_schema_3() {
        let missing_class =
            liteinst_json(3, "control", 0, 400).replace(r#""engagement_class":"control","#, "");
        let error = serde_json::from_str::<BackendEngagementReport>(&missing_class).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing field `engagement_class`")
        );

        let wrong_denominator: BackendEngagementReport =
            serde_json::from_str(&liteinst_json(3, "patched_via_ptrace", 9, 399)).unwrap();
        assert_eq!(
            wrong_denominator.validate().unwrap_err(),
            "liteinst patched_site_entries denominator 399 is not syscall_entries 400"
        );

        let old_schema: BackendEngagementReport =
            serde_json::from_str(&liteinst_json(2, "control", 0, 400)).unwrap();
        assert_eq!(
            old_schema.validate().unwrap_err(),
            "a liteinst backend-engagement record requires schema 3, got 2"
        );
    }

    #[test]
    fn liteinst_host_counters_cannot_exceed_the_tool_entry_count() {
        assert_eq!(
            BackendEngagement::liteinst_host_hybrid(10, counters(7)).unwrap_err(),
            "liteinst patched-site (7) plus first-site (4) entries exceed syscall_entries 10"
        );
        BackendEngagement::liteinst_host_hybrid(11, counters(7)).unwrap();
    }
    #[test]
    fn e9patch_result_requires_all_three_counts() {
        for incomplete in [
            r#"{"schema":2,"engagement":{"backend":"e9patch","mapped_sites":7,"b0_sites":0}}"#,
            r#"{"schema":2,"engagement":{"backend":"e9patch","candidate_sites":7,"b0_sites":0}}"#,
            r#"{"schema":2,"engagement":{"backend":"e9patch","candidate_sites":7,"mapped_sites":7}}"#,
        ] {
            assert!(serde_json::from_str::<BackendEngagementReport>(incomplete).is_err());
        }
    }
}
