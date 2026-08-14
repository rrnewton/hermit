//! Establish and verify the outer safe-ci scope used by in-process DAG clients.

use std::sync::Arc;

use safe_ci_dag_runner::cgroup::CgroupManager;
use safe_ci_dag_runner::cgroup::Cgroups;
use safe_ci_dag_runner::cgroup::attempt_scope_reexec;
use safe_ci_dag_runner::cgroup::enable_outer_oom_group;
use safe_ci_dag_runner::cgroup::expected_outer_memory_max_bytes;
use safe_ci_dag_runner::cgroup::expected_scope_runtime_max_s;
use safe_ci_dag_runner::cgroup::install_scope_teardown;
use safe_ci_dag_runner::cgroup::is_in_scope;
use safe_ci_dag_runner::cgroup::verify_scope_limits;
use safe_ci_dag_runner::cgroup::verify_scope_runtime_max;
use safe_ci_dag_runner::scheduler::BoxedCgroups;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScopeRequirement {
    LiveContainment,
    OuterMemorySwapAndOomGroup,
    RuntimeMax,
    PerStepCgroups,
}

fn requirement_message(requirement: ScopeRequirement) -> &'static str {
    match requirement {
        ScopeRequirement::LiveContainment => {
            "the in-scope claim is not supported by the live process"
        }
        ScopeRequirement::OuterMemorySwapAndOomGroup => {
            "outer MemoryMax/MemorySwapMax/memory.oom.group readback failed"
        }
        ScopeRequirement::RuntimeMax => "this invocation's outer RuntimeMaxSec readback failed",
        ScopeRequirement::PerStepCgroups => {
            "the observed outer scope could not create per-step cgroups"
        }
    }
}

fn require_observed(requirement: ScopeRequirement, observed: bool) -> Result<(), &'static str> {
    if observed {
        Ok(())
    } else {
        Err(requirement_message(requirement))
    }
}

fn runtime_readback_satisfies(requirement_is_active: bool, observed: bool) -> bool {
    !requirement_is_active || observed
}

#[derive(Clone, Copy, Debug)]
struct ScopeObservations {
    live_containment: bool,
    outer_memory_swap_and_oom_group: bool,
    runtime_max: bool,
    per_step_cgroups: bool,
}

/// The same ordered refusal decision used by the live path and the inert
/// bracket. Keeping the observations as typed inputs makes bypassing any one of
/// them observable without pretending a unit test can manufacture a live
/// systemd scope.
fn require_scope_observations(
    observations: ScopeObservations,
    verify_runtime: bool,
) -> Result<(), &'static str> {
    require_observed(
        ScopeRequirement::LiveContainment,
        observations.live_containment,
    )?;
    require_observed(
        ScopeRequirement::OuterMemorySwapAndOomGroup,
        observations.outer_memory_swap_and_oom_group,
    )?;
    require_observed(
        ScopeRequirement::RuntimeMax,
        runtime_readback_satisfies(verify_runtime, observations.runtime_max),
    )?;
    require_observed(
        ScopeRequirement::PerStepCgroups,
        observations.per_step_cgroups,
    )?;
    Ok(())
}

fn unavailable(label: &str, allow_failure: bool, message: &str) -> Result<BoxedCgroups, u8> {
    if allow_failure {
        eprintln!("{label}: WARNING: {message}; running UNBOXED (--allow-cgroup-failure).");
        Ok(None)
    } else {
        eprintln!("{label}: ERROR: {message}.");
        Err(3)
    }
}

/// Keep the helper's typed refusal load-bearing at both Rust call sites. The
/// self-test feeds this an Err(3), so replacing it with a discarded result and
/// an unboxed success is behaviorally detected.
pub fn propagate_result(result: Result<BoxedCgroups, u8>) -> Result<BoxedCgroups, u8> {
    result
}

/// Establish two-level cgroup-v2 boxing for a direct Rust scheduler client.
///
/// A successful initial call re-executes the current CLI inside a transient
/// scope. The in-scope call verifies the running process and every requested
/// outer limit before returning a per-step cgroup manager. `verify_runtime`
/// is false only when the caller inherited somebody else's scope rather than
/// requesting its own `RuntimeMaxSec` rung.
pub fn resolve_cgroups(
    label: &str,
    allow_failure: bool,
    scope_runtime_s: Option<i64>,
    verify_runtime: bool,
) -> Result<BoxedCgroups, u8> {
    if !is_in_scope() {
        if allow_failure {
            return unavailable(
                label,
                true,
                "cgroup boxing was not established; process-group teardown and per-step wall limits remain active",
            );
        }
        let attempt = attempt_scope_reexec(None, None, scope_runtime_s);
        return unavailable(
            label,
            false,
            &format!(
                "cgroup boxing could not be established: {}; resource boxing is required",
                attempt.describe()
            ),
        );
    }

    let attempt = attempt_scope_reexec(None, None, None);
    let Some(memory_max) = expected_outer_memory_max_bytes() else {
        return unavailable(
            label,
            allow_failure,
            "the outer scope did not carry its requested MemoryMax",
        );
    };
    let outer_limits_observed = enable_outer_oom_group() && verify_scope_limits(memory_max);
    let runtime_observed =
        !verify_runtime || expected_scope_runtime_max_s().is_some_and(verify_scope_runtime_max);
    let manager = Cgroups::new();
    let observations = ScopeObservations {
        live_containment: attempt.is_contained(),
        outer_memory_swap_and_oom_group: outer_limits_observed,
        runtime_max: runtime_observed,
        per_step_cgroups: manager.enabled(),
    };
    if let Err(message) = require_scope_observations(observations, verify_runtime) {
        let detail = if !observations.live_containment {
            format!("{message}: {}", attempt.describe())
        } else {
            message.to_string()
        };
        return unavailable(label, allow_failure, &detail);
    }
    install_scope_teardown();
    eprintln!(
        "{label}: cgroup boxing ACTIVE; containment and outer limits OBSERVED: {}.",
        attempt.describe()
    );
    Ok(Some(Arc::new(manager) as Arc<dyn CgroupManager>))
}

/// Inert two-sided checks for the decisions made from live cgroup observations.
pub fn self_test() -> Result<String, String> {
    let all = ScopeObservations {
        live_containment: true,
        outer_memory_swap_and_oom_group: true,
        runtime_max: true,
        per_step_cgroups: true,
    };
    require_scope_observations(all, true).map_err(str::to_owned)?;
    for (requirement, observations) in [
        (
            ScopeRequirement::LiveContainment,
            ScopeObservations { live_containment: false, ..all },
        ),
        (
            ScopeRequirement::OuterMemorySwapAndOomGroup,
            ScopeObservations {
                outer_memory_swap_and_oom_group: false,
                ..all
            },
        ),
        (
            ScopeRequirement::RuntimeMax,
            ScopeObservations { runtime_max: false, ..all },
        ),
        (
            ScopeRequirement::PerStepCgroups,
            ScopeObservations { per_step_cgroups: false, ..all },
        ),
    ] {
        let refused = require_scope_observations(observations, true)
            .expect_err("a missing required observation must refuse");
        if refused != requirement_message(requirement) {
            return Err(format!(
                "cgroup requirement {requirement:?} refused with the wrong reason: {refused}"
            ));
        }
    }
    // The production helper converts a missing observation into Err(3) on the
    // default fail-closed path; accepting Ok(None) is reserved for the explicit
    // allow-failure diagnostic mode.
    if !matches!(
        unavailable("safe-ci scope self-test", false, "planted refusal"),
        Err(3)
    ) {
        return Err("a required observation did not propagate as fail-closed exit 3".into());
    }
    if !matches!(
        unavailable("safe-ci scope self-test", true, "planted refusal"),
        Ok(None)
    ) {
        return Err("explicit allow-failure mode did not remain an unboxed warning".into());
    }
    if !matches!(propagate_result(Err(3)), Err(3)) {
        return Err("the caller-facing helper result did not preserve fail-closed exit 3".into());
    }
    if !runtime_readback_satisfies(false, false)
        || !runtime_readback_satisfies(true, true)
        || runtime_readback_satisfies(true, false)
    {
        return Err(
            "RuntimeMax readback did not distinguish an inherited scope from an invocation-owned request"
                .into(),
        );
    }
    Ok(
        "safe-ci scope: containment, outer memory/swap/OOM-group, optional RuntimeMax, and per-step cgroups bracketed"
            .into(),
    )
}
