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

fn unavailable(label: &str, allow_failure: bool, message: &str) -> Result<BoxedCgroups, u8> {
    if allow_failure {
        eprintln!("{label}: WARNING: {message}; running UNBOXED (--allow-cgroup-failure).");
        Ok(None)
    } else {
        eprintln!("{label}: ERROR: {message}.");
        Err(3)
    }
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
    if let Err(message) =
        require_observed(ScopeRequirement::LiveContainment, attempt.is_contained())
    {
        return unavailable(
            label,
            allow_failure,
            &format!("{message}: {}", attempt.describe()),
        );
    }

    let Some(memory_max) = expected_outer_memory_max_bytes() else {
        return unavailable(
            label,
            allow_failure,
            "the outer scope did not carry its requested MemoryMax",
        );
    };
    let outer_limits_observed = enable_outer_oom_group() && verify_scope_limits(memory_max);
    if let Err(message) = require_observed(
        ScopeRequirement::OuterMemorySwapAndOomGroup,
        outer_limits_observed,
    ) {
        return unavailable(label, allow_failure, message);
    }

    let runtime_observed =
        !verify_runtime || expected_scope_runtime_max_s().is_some_and(verify_scope_runtime_max);
    if let Err(message) = require_observed(
        ScopeRequirement::RuntimeMax,
        runtime_readback_satisfies(verify_runtime, runtime_observed),
    ) {
        return unavailable(label, allow_failure, message);
    }

    let manager = Cgroups::new();
    if let Err(message) = require_observed(ScopeRequirement::PerStepCgroups, manager.enabled()) {
        return unavailable(label, allow_failure, message);
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
    for requirement in [
        ScopeRequirement::LiveContainment,
        ScopeRequirement::OuterMemorySwapAndOomGroup,
        ScopeRequirement::RuntimeMax,
        ScopeRequirement::PerStepCgroups,
    ] {
        require_observed(requirement, true).map_err(str::to_owned)?;
        let refused = require_observed(requirement, false)
            .expect_err("a missing required observation must refuse");
        if refused != requirement_message(requirement) {
            return Err(format!(
                "cgroup requirement {requirement:?} refused with the wrong reason: {refused}"
            ));
        }
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
