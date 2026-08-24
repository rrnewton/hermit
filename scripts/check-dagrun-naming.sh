#!/usr/bin/env bash
# Keep hermit's DAG-runner naming in agreement with the pinned agent-utils.
#
# agent-utils renamed its DAG runner: the crate rs/safe-ci-dag-runner became
# rs/dagrun, the package became `dagrun`, the executables under common/bin, py/bin
# and rs/bin became `dagrun`, the per-checkout profile store moved from
# .safe-ci-dag-runner/profiles to .dagrun/profiles, and every runner-read
# environment variable took a DAGRUN_ prefix.
#
# That rename arrives through a submodule pin bump, so nothing in hermit's own
# build fails when a hermit-side reference goes stale. The two ways it goes wrong
# are both quiet:
#
#   * A retired executable or path name is looked up, is simply absent, and the
#     lane reports "not found" instead of running. This is what took main down at
#     4b9a56bfc2 -- cargo build, test and clippy all stayed green while no DAG
#     node could execute at all.
#   * A retired SAFE_CI_DAG_RUNNER_* variable is still exported. Nothing reads it,
#     nothing errors, and the setting silently stops applying.
#
# So this gate enforces two things.
#
# CHECK 1 -- the retired spellings are at zero in forward-looking files.
#
# CHECK 2 -- the ONE surviving old-prefix constant still agrees with the pin.
# SAFE_CI_DAG_RUNNER_JOBS_ENV was added after the rename and kept the old prefix,
# so it is the single legitimate occurrence and check 1 allows it. It is also the
# most fragile thing here: it is the channel through which a DAG node's declared
# build width reaches Cargo, and the moment agent-utils normalises it to
# DAGRUN_JOBS_ENV, hermit's export is read by nothing, no error is raised, and
# every build quietly falls back to the ambient 8. The width fix would revert
# itself in silence. Check 2 makes that a loud failure instead.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT_DIR
cd "$ROOT_DIR"

# The one spelling that is still correct, because the pinned agent-utils still
# reads it. Check 2 is what proves that claim rather than assuming it.
readonly LIVE_CONSTANT='SAFE_CI_DAG_RUNNER_JOBS_ENV'

# The retired surface is the crate/executable/profile-store name in either
# spelling, plus the whole retired SAFE_CI_ environment prefix. The prefix is in
# scope deliberately: a first sweep that looked only for SAFE_CI_DAG_RUNNER_*
# missed five live variables (SAFE_CI_IN_SCOPE, SAFE_CI_SCOPE_UNIT,
# SAFE_CI_FORCE_SCOPE_ATTEMPT, SAFE_CI_EXPECTED_OUTER_MEMORY_MAX_BYTES,
# SAFE_CI_EXPECTED_RUNTIME_MAX_SEC) that the rename moved to DAGRUN_ and that
# hermit was still setting and clearing under names nothing reads.
#
# NOT in scope: hermit's own lowercase `safe_ci_scope` module and its `[safe-ci]`
# log labels. Those are hermit-local names that read no environment and resolve
# no path, so they break nothing; renaming them is cosmetic churn, not this fix.
readonly RETIRED_RE='safe-ci-dag-runner|safe_ci_dag_runner|SAFE_CI_[A-Z_]*'

# Historical evidence legitimately records the old name: ledger rows, receipts,
# captured logs and past experiment output are append-only and must NOT be
# rewritten. Only forward-looking code and CI plumbing is in scope.
is_excluded() {
    case "/$1" in
        */.git/* | */ignored/* | */experiments/* | */scratch/* | */target/* \
            | */third-party/* | */agent-utils/* | */docs/archive/* \
            | "/scripts/check-dagrun-naming.sh")
            return 0 ;;
        *) return 1 ;;
    esac
}

status=0

# ---------------------------------------------------------------- check 1
offenders=()
while IFS= read -r path; do
    is_excluded "$path" && continue
    [[ -f $path ]] || continue
    # Drop the permitted constant before looking for the retired spellings, so a
    # line carrying only SAFE_CI_DAG_RUNNER_JOBS_ENV does not trip the scan.
    hits=$(sed "s/${LIVE_CONSTANT}//g" -- "$path" \
        | grep -nE "$RETIRED_RE" || true)
    if [[ -n $hits ]]; then
        while IFS= read -r line; do
            offenders+=("$path:$line")
        done <<<"$hits"
    fi
done < <(git ls-files -z | tr '\0' '\n')

if ((${#offenders[@]} > 0)); then
    echo "check-dagrun-naming: retired DAG-runner spellings found in tracked files." >&2
    echo "  agent-utils renamed safe-ci-dag-runner to dagrun; these references resolve" >&2
    echo "  to nothing at the current pin, and most fail silently rather than loudly." >&2
    echo >&2
    printf '    %s\n' "${offenders[@]}" >&2
    echo >&2
    echo "  Rename them to the dagrun spelling. The only permitted occurrence is the" >&2
    echo "  constant ${LIVE_CONSTANT}, which the pinned agent-utils still reads." >&2
    status=1
fi

# ---------------------------------------------------------------- check 2
readonly MODEL_PY='agent-utils/py/dagrun/model.py'
readonly EXPORTER='ci/configure-build-jobs.sh'

if [[ ! -f $MODEL_PY ]]; then
    # Distinguish this from a real naming failure. An unpopulated submodule
    # produces the same "missing" shape for an entirely different reason, and
    # confusing the two has already cost a diagnosis cycle.
    echo "check-dagrun-naming: $MODEL_PY is absent -- the agent-utils submodule is not" >&2
    echo "  populated, so the constant-agreement check cannot run. This is a checkout" >&2
    echo "  problem, NOT a naming problem. Run: git submodule update --init agent-utils" >&2
    exit 2
fi

pinned=$(sed -n 's/^JOBS_ENV_ENV[[:space:]]*=[[:space:]]*"\([A-Z_]*\)".*/\1/p' "$MODEL_PY")
exported=$(sed -n 's/^[[:space:]]*export[[:space:]]\{1,\}\([A-Z_]*\)=CARGO_BUILD_JOBS.*/\1/p' "$EXPORTER")

if [[ -z $pinned ]]; then
    echo "check-dagrun-naming: could not read JOBS_ENV_ENV from $MODEL_PY." >&2
    echo "  The runner's width channel has been restructured; re-derive it and update" >&2
    echo "  both this gate and $EXPORTER together." >&2
    status=1
elif [[ -z $exported ]]; then
    echo "check-dagrun-naming: $EXPORTER no longer exports a width-channel variable" >&2
    echo "  set to CARGO_BUILD_JOBS. A DAG node's declared width can no longer reach Cargo." >&2
    status=1
elif [[ $pinned != "$exported" ]]; then
    echo "check-dagrun-naming: the build-width channel is broken." >&2
    echo "  $EXPORTER exports: $exported" >&2
    echo "  $MODEL_PY reads:   $pinned" >&2
    echo >&2
    echo "  These must be the same string. They are not, so the per-node build width is" >&2
    echo "  being exported into a variable nothing reads, and every step has silently" >&2
    echo "  fallen back to the ambient default. Rename the export to match the pin." >&2
    status=1
fi

if ((status == 0)); then
    echo "check-dagrun-naming: OK (retired spellings at zero; width channel = $pinned)"
fi

exit "$status"
