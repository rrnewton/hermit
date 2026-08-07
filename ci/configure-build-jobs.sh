#!/usr/bin/env bash
# Shared inner-build width for every Hermit CI DAG launch path.
#
# The outer safe-ci cpu.max is a containment ceiling, not a request for Cargo to
# use every granted core. On the 316-CPU validation host that inference produced
# NUM_JOBS=284 and raced the native linker. K=8 is measurement-backed: on
# 2026-08-04 the pre-collapse build.dbi_release and rr_suite_contract nodes both
# completed at j8 under their cgroup-recorded memory caps. The collapsed fat-build
# nodes declare their independently measured higher width in the DAG manifest.
#
# This file has two explicit source modes. `launcher` preserves the historical
# shared Cargo widths and strips every portable DBI-budget variable before the
# DAG runner starts. `reverie-dbi-budget-child` is called only by the portable
# DBI wrapper, after safe-ci has entered the child and selected any child-local
# Cargo width.

CI_DAG_BUILD_JOBS=${CI_DAG_BUILD_JOBS:-8}
if [[ ! $CI_DAG_BUILD_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: CI_DAG_BUILD_JOBS must be a positive integer" >&2
    return 2
fi

build_job_context=${1:-}
if [[ $build_job_context == launcher ]]; then
    # These variables are meaningful only in the two portable DBI build
    # children. Remove even planted ambient values so the privileged runner's
    # environment remains identical to the pre-budget launcher contract.
    unset REVERIE_DBI_BUDGET_BOUND_PIN
    unset REVERIE_DBI_BUILD_JOBS_SOURCE
    unset REVERIE_DBI_RAW_BUILD_JOBS
    unset REVERIE_DBI_EFFECTIVE_CPUS_SOURCE
    unset REVERIE_DBI_EFFECTIVE_CPUS
    unset REVERIE_DBI_MAX_PARALLEL_JOBS
    unset REVERIE_DBI_EFFECTIVE_BUILD_JOBS
    unset REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS
    unset REVERIE_DBI_MAX_BUILD_SECONDS

    # Retire the previous launcher-carried derivation names fail-closed too.
    unset CI_DAG_LAUNCH_WIDTH_BOUND
    unset CI_DAG_LAUNCH_BUILD_JOBS_SOURCE
    unset CI_DAG_LAUNCH_RAW_BUILD_JOBS
    unset CI_DAG_EFFECTIVE_CPUS
    unset CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS
    unset CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS
    unset CI_DAG_REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS
    unset REVERIE_DBI_PINNED_MAX_PARALLEL_JOBS
    unset REVERIE_DBI_BUDGET_CHILD

    # Cargo converts this explicit pool width into build-script NUM_JOBS. Keep
    # the nested native-build knob identical so validate.sh cannot widen it.
    export CARGO_BUILD_JOBS=$CI_DAG_BUILD_JOBS
    export THIRD_PARTY_BUILD_JOBS=$CI_DAG_BUILD_JOBS
    return 0
fi

if [[ $build_job_context != reverie-dbi-budget-child ]]; then
    echo "configure-build-jobs.sh: expected source mode launcher or reverie-dbi-budget-child" >&2
    return 2
fi

# fc97 briefly exported this unconditioned threshold before the budget was
# normalized to effective-job-seconds. A direct wrapper invocation must not
# carry that retired authority into Cargo; normal launchers scrub it above.
if [[ -v CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS ]]; then
    echo "configure-build-jobs.sh: retired CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS is not accepted in a DBI budget child" >&2
    return 2
fi

# Independently re-derive the repository's one recorded Reverie pin. The
# wrapper carries that value with the budget tuple, while this check prevents a
# direct or stale caller from substituting a different well-formed SHA. The
# canonical scanner is the only pin authority; never mirror its result here.
budget_repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if ! budget_recorded_pin=$(
    "$budget_repo_root/ci/run-reverie-pin-check.sh" \
        --repo "$budget_repo_root" --print-pin
); then
    echo "configure-build-jobs.sh: could not derive the recorded Reverie pin" >&2
    return 2
fi
if [[ ${REVERIE_DBI_BUDGET_BOUND_PIN:-} != "$budget_recorded_pin" ]]; then
    echo "configure-build-jobs.sh: DBI budget pin ${REVERIE_DBI_BUDGET_BOUND_PIN:-<unset>} does not match recorded Reverie pin $budget_recorded_pin" >&2
    return 2
fi

# Carry the measured threshold with the condition that makes it valid. The
# pin is not that condition: it changed repeatedly while the DynamoRIO recipe
# stayed byte-identical. reverie-dbi/build.rs computes its recipe key from its
# own blob, vendor/dynamorio, CMAKE, and CMAKE_GENERATOR. Resolve the exact
# package selected by locked Cargo metadata and compare those inputs before the
# budget can reach the command. These calibrated object ids are stable recipe
# identities, not a second list of moving Reverie pins.
REVERIE_DBI_CALIBRATED_RECIPE_KEY=76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d
REVERIE_DBI_CALIBRATED_BUILD_RS_OBJECT=9e35e1b699b76d8b9f8a6adacc21c7a095f4f8f7
REVERIE_DBI_CALIBRATED_VENDOR_DYNAMORIO_OBJECT=de352475846e385002c1e4e54604fa0a7647b2de
REVERIE_DBI_CALIBRATED_BASIS=github-portable-cold-miss-n3-affinity4

if ! budget_metadata=$(cargo metadata --format-version 1 --locked \
    --manifest-path "$budget_repo_root/Cargo.toml"); then
    echo "configure-build-jobs.sh: could not resolve locked Cargo metadata for the DBI budget" >&2
    return 2
fi
if ! budget_dbi_manifest=$(jq -er '
    [.packages[] | select(.name == "reverie-dbi") | .manifest_path]
    | unique
    | if length == 1 then .[0] else error("expected exactly one reverie-dbi package") end
' <<<"$budget_metadata"); then
    echo "configure-build-jobs.sh: locked Cargo metadata did not identify exactly one reverie-dbi package" >&2
    return 2
fi
if ! budget_reverie_root=$(git -C "$(dirname -- "$budget_dbi_manifest")" rev-parse --show-toplevel) ||
    ! budget_package_pin=$(git -C "$budget_reverie_root" rev-parse HEAD) ||
    ! budget_build_rs_object=$(git -C "$budget_reverie_root" rev-parse HEAD:reverie-dbi/build.rs) ||
    ! budget_vendor_object=$(git -C "$budget_reverie_root" rev-parse HEAD:reverie-dbi/vendor/dynamorio); then
    echo "configure-build-jobs.sh: could not derive the selected Reverie DBI recipe inputs" >&2
    return 2
fi

budget_recipe_mismatch=0
if [[ $budget_package_pin != "$budget_recorded_pin" ]]; then
    echo "configure-build-jobs.sh: Cargo selected Reverie $budget_package_pin, not recorded pin $budget_recorded_pin" >&2
    budget_recipe_mismatch=1
fi
if [[ $budget_build_rs_object != "$REVERIE_DBI_CALIBRATED_BUILD_RS_OBJECT" ]]; then
    echo "configure-build-jobs.sh: reverie-dbi/build.rs object $budget_build_rs_object does not match calibrated object $REVERIE_DBI_CALIBRATED_BUILD_RS_OBJECT" >&2
    budget_recipe_mismatch=1
fi
if [[ $budget_vendor_object != "$REVERIE_DBI_CALIBRATED_VENDOR_DYNAMORIO_OBJECT" ]]; then
    echo "configure-build-jobs.sh: reverie-dbi/vendor/dynamorio object $budget_vendor_object does not match calibrated object $REVERIE_DBI_CALIBRATED_VENDOR_DYNAMORIO_OBJECT" >&2
    budget_recipe_mismatch=1
fi
if [[ ${CMAKE-cmake} != cmake ]]; then
    echo "configure-build-jobs.sh: CMAKE=${CMAKE} changes calibrated DynamoRIO recipe key $REVERIE_DBI_CALIBRATED_RECIPE_KEY" >&2
    budget_recipe_mismatch=1
fi
if [[ -v CMAKE_GENERATOR ]]; then
    echo "configure-build-jobs.sh: CMAKE_GENERATOR changes calibrated DynamoRIO recipe key $REVERIE_DBI_CALIBRATED_RECIPE_KEY" >&2
    budget_recipe_mismatch=1
fi
if ((budget_recipe_mismatch != 0)); then
    echo "configure-build-jobs.sh: DBI budget recipe mismatch; re-measure before applying its threshold" >&2
    return 2
fi
REVERIE_DBI_BUDGET_RECIPE_BINDING=locked-cargo-git-object-identities
unset budget_build_rs_object budget_dbi_manifest budget_metadata
unset budget_package_pin budget_recipe_mismatch budget_recorded_pin
unset budget_repo_root budget_reverie_root budget_vendor_object

if [[ -n ${CARGO_BUILD_JOBS:-} ]]; then
    REVERIE_DBI_RAW_BUILD_JOBS=$CARGO_BUILD_JOBS
    if [[ ${SAFE_CI_IN_SCOPE:-} == 1 ]]; then
        REVERIE_DBI_BUILD_JOBS_SOURCE=runner-child-cargo-build-jobs
    else
        REVERIE_DBI_BUILD_JOBS_SOURCE=inherited-launch-cargo-build-jobs
    fi
else
    REVERIE_DBI_RAW_BUILD_JOBS=$CI_DAG_BUILD_JOBS
    REVERIE_DBI_BUILD_JOBS_SOURCE=ci-dag-build-jobs-fallback
fi
if [[ ! $REVERIE_DBI_RAW_BUILD_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: selected raw build width must be a positive integer" >&2
    return 2
fi

# Observe affinity/cpuset visibility in this child, after safe-ci has applied
# its containment. A launcher observation would be only a correlated proxy for
# the CPUs available to the native build.
if ! REVERIE_DBI_EFFECTIVE_CPUS=$(nproc); then
    echo "configure-build-jobs.sh: child nproc observation failed" >&2
    return 2
fi
REVERIE_DBI_EFFECTIVE_CPUS_SOURCE=child-nproc
if [[ ! $REVERIE_DBI_EFFECTIVE_CPUS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: child nproc must return a positive integer" >&2
    return 2
fi

# Reverie 9470712's DynamoRIO build.rs clamps Cargo NUM_JOBS to 16 before
# passing it to `cmake --parallel`. Carry the calibrated threshold together with
# every condition used to convert it into elapsed seconds:
#
#   effective native jobs = min(requested jobs, child CPUs, Reverie clamp)
#   max elapsed seconds = ceil(effective-job-second threshold / effective jobs)
#
# PROVENANCE (GitHub portable run 31008044311 at Hermit f21b22ed, requested
# jobs=8, runner affinity=4): three content-key misses measured 115.82s,
# 128.27s, and 131.21s -- one debug build and two concurrent release builds --
# i.e. 463.28, 513.08, and 524.84 effective-job-seconds at min(8, 4, 16)=4.
# Reverie's original ratchet policy used 2x the slowest of n=3 clean
# observations; applying that policy and rounding up gives 1050
# effective-job-seconds. The concurrent release builds embody contention;
# replace this calibration when >=5 clean Hermit-lane samples support it.
#
# CARRY TO 9470712 (2026-08-05). The threshold above was measured at 025d378
# and is reused here, so the reuse is evidenced rather than assumed. The budget
# governs exactly one quantity: the elapsed time reverie-dbi/build.rs reports
# for a DynamoRIO content-key MISS. That build's inputs are hashed by
# source_recipe_key() over {reverie-dbi/vendor/dynamorio, reverie-dbi/build.rs,
# $CMAKE, $CMAKE_GENERATOR} -- host-invariant while CMAKE/CMAKE_GENERATOR are
# unset -- and six cold builds (three per pin, interleaved on one host,
# taskset 4 CPUs, CARGO_BUILD_JOBS=4) all printed the SAME recipe key
# sha256:19123c88d87a4cd9e8b0efdda7265c7682e8907fe6bbf8e0bd6fcb92fbfa85e4.
# Elapsed at 9470712: 39.80s / 39.23s / 39.52s (159.20 / 156.92 / 158.08
# effective-job-seconds); at 025d378: 38.10s / 39.58s / 41.01s (152.40 /
# 158.32 / 164.04). The new pin's slowest sample is 3% faster than the old
# pin's slowest and the whole set spans 7.1%, so the pin move causes no
# throughput change. Corroborating Git evidence: 025d378..9470712 touches only
# reverie-ptrace/src/{error,task,tracer}.rs; the reverie-dbi subtree
# (c38c979057f9fe3e4d46772c1fddd05a71db4bf9) and third-party/
# (fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a) are identical at both pins, and
# MAX_PARALLEL_JOBS is still 16.
#
# CARRY TO e159d6c (2026-08-06). The only 9470712..e159d6c change is a
# hostname-neutral wording edit in reverie-dbi/build.rs. The vendored
# DynamoRIO tree, build commands, MAX_PARALLEL_JOBS=16 clamp, and
# CI_MAX_BUILD_JOB_SECONDS=572 remain identical. Because source_recipe_key()
# deliberately hashes the full build script, its default-tool identity changes
# to sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d.
# A cold local CARGO_BUILD_JOBS=4 check observed the new identity and completed
# its native build in 30.73s (122.92 effective-job-seconds). This confirms the
# identity transition but does not replace the slower GitHub-runner calibration.
#
# CARRY TO 6a6b4ec (2026-08-06). The e159d6c..6a6b4ec changes are confined to
# reverie-kvm task lifecycle, process-tree exit accounting, and KVM tests.
# reverie-dbi/build.rs, its vendored DynamoRIO tree, build commands, and the
# MAX_PARALLEL_JOBS=16 clamp are byte-identical, so source_recipe_key() remains
# sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d.
# CI_MAX_BUILD_JOB_SECONDS=572 and the measured hosted-runner budget therefore
# carry without changing the derivation.
#
# CARRY TO dd3c178 (2026-08-06). The only 6a6b4ec..dd3c178 change adds
# reverie-kvm sendmsg/recvmsg ancillary-data translation and KVM tests.
# reverie-dbi/build.rs, its vendored DynamoRIO tree, build commands, and the
# MAX_PARALLEL_JOBS=16 clamp remain byte-identical. The DBI recipe identity
# therefore remains sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d,
# and the hosted-runner budget carries unchanged.
#
# CARRY TO 0ae0c01 (2026-08-06). dd3c178..0ae0c01 is rrnewton/reverie#396,
# which revives the KVM backend: it stops answering the `Guest::ppid`
# traced-tree contract from the guest-visible getppid() value, so Detcore
# registers the root thread again. Before it, every `hermit run --backend kvm`
# hung before the first guest syscall, including /bin/true.
#
# `git diff --name-only dd3c178..0ae0c01` is exactly two files, both KVM:
#   reverie-kvm/src/elf.rs
#   reverie-kvm/src/executor.rs
# The DBI inputs are byte-identical by git object identity at both pins --
# reverie-dbi/build.rs 9e35e1b699b7, reverie-dbi/vendor/dynamorio de352475846e,
# third-party fb49c0ba7a9a, and the whole reverie-dbi subtree eb284556d2df --
# so source_recipe_key() is unchanged at
# sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d and
# the MAX_PARALLEL_JOBS=16 clamp still applies. The hosted-runner budget
# therefore carries without re-derivation. This carry is evidenced by tree
# identity rather than by a fresh timing run, exactly as the 6a6b4ec and
# dd3c178 carries above: no DBI build input changed, so there is nothing for a
# new timing sample to measure.
#
# CARRY TO 6144323 (2026-08-07). 0ae0c01..6144323 is exactly one commit,
# rrnewton/reverie#377 (HybridPtrace A-class lifecycle-owner for reverie-e9patch),
# touching 8 files: reverie-e9patch/{README.md,src/backend.rs,src/lib.rs,
# src/runtime.rs}, reverie-preload/{README.md,src/lifecycle.rs}, and
# reverie-ptrace/{src/tracer.rs,tests/stdio_drain.rs}. NONE is a DBI input.
#
# Verified by git object identity at both pins, not by inspection: build.rs
# 9e35e1b699b7, vendor/dynamorio de352475846e, third-party fb49c0ba7a9a, and the
# whole reverie-dbi subtree eb284556d2df are byte-identical at 0ae0c01 and at
# 6144323 -- the same four object ids this file already records for 0ae0c01, so
# the recorded evidence for the previous carry independently checks out too.
# source_recipe_key() is therefore unchanged at
# sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d and the
# MAX_PARALLEL_JOBS=16 clamp (reverie-dbi/build.rs:25) still applies, so the
# hosted-runner budget carries without re-derivation. Evidenced by tree identity
# rather than a fresh timing run, exactly as the 6a6b4ec, dd3c178 and 0ae0c01
# carries above: no DBI build input changed, so there is nothing to re-measure.
#
# Those 2026-08-05 samples deliberately do NOT replace 1050. They come from a
# development host whose cores finish the identical work ~3.3x faster than the
# GitHub portable runner this budget governs; 2x their slowest would give 319
# effective-job-seconds and would fail the portable lane on its first genuine
# cold miss. The replacement bar stated above -- >=5 clean Hermit-lane samples
# -- is unchanged and still unmet.
REVERIE_DBI_MAX_PARALLEL_JOBS=16
REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS=1050
REVERIE_DBI_EFFECTIVE_BUILD_JOBS=$REVERIE_DBI_RAW_BUILD_JOBS
if ((REVERIE_DBI_EFFECTIVE_CPUS < REVERIE_DBI_EFFECTIVE_BUILD_JOBS)); then
    REVERIE_DBI_EFFECTIVE_BUILD_JOBS=$REVERIE_DBI_EFFECTIVE_CPUS
fi
if ((REVERIE_DBI_MAX_PARALLEL_JOBS < REVERIE_DBI_EFFECTIVE_BUILD_JOBS)); then
    REVERIE_DBI_EFFECTIVE_BUILD_JOBS=$REVERIE_DBI_MAX_PARALLEL_JOBS
fi
REVERIE_DBI_MAX_BUILD_SECONDS=$((
    (REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS +
        REVERIE_DBI_EFFECTIVE_BUILD_JOBS - 1) /
        REVERIE_DBI_EFFECTIVE_BUILD_JOBS
))

export CARGO_BUILD_JOBS=$REVERIE_DBI_RAW_BUILD_JOBS
export THIRD_PARTY_BUILD_JOBS=$REVERIE_DBI_RAW_BUILD_JOBS
export REVERIE_DBI_BUDGET_BOUND_PIN
export REVERIE_DBI_BUILD_JOBS_SOURCE
export REVERIE_DBI_RAW_BUILD_JOBS
export REVERIE_DBI_EFFECTIVE_CPUS_SOURCE
export REVERIE_DBI_EFFECTIVE_CPUS
export REVERIE_DBI_MAX_PARALLEL_JOBS
export REVERIE_DBI_EFFECTIVE_BUILD_JOBS
export REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS
export REVERIE_DBI_MAX_BUILD_SECONDS
