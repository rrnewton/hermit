#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Focused live bracket for the mount-namespace helper and its canonical wrapper
# routing. It requires passwordless sudo and a built Hermit binary, but runs no
# manifest matrix and publishes no validation evidence.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT_DIR
readonly HELPER=$ROOT_DIR/ci/run-with-private-test-root.sh
readonly WRAPPER=$ROOT_DIR/ci/run-with-hermit-e2e-artifact.sh
readonly MARKER=HERMIT_VALIDATE_PRIVATE_TEST_ROOT
readonly MARKER_VALUE=canonical-full-manifest-v1
readonly HERMIT_UNDER_TEST=${HERMIT_BIN_UNDER_TEST:-$ROOT_DIR/target/debug/hermit}
readonly REGRESSION_ROOT=${PRIVATE_TEST_ROOT_REGRESSION_ROOT:-$ROOT_DIR}

fail() {
    printf 'private-test-root-test.sh: %s\n' "$*" >&2
    exit 1
}

[[ ! -e /test ]] || fail "precondition failed: host /test already exists"
[[ -x $HERMIT_UNDER_TEST ]] ||
    fail "Hermit is not built: $HERMIT_UNDER_TEST (set HERMIT_BIN_UNDER_TEST to an executable)"

scratch=$(mktemp -d /tmp/hermit-private-test-root-test.XXXXXX)
readonly scratch
cleanup() {
    status=$?
    if [[ -e /test ]]; then
        printf 'private-test-root-test.sh: host /test leaked from the private namespace\n' >&2
        status=1
    fi
    rm -rf -- "$scratch"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

set +e
refusal=$(env -u "$MARKER" "$HELPER" -- /bin/true 2>&1)
refusal_status=$?
set -e
[[ $refusal_status == 2 && $refusal == *'refusing outside an admitted top-level full-profile manifest step'* ]] ||
    fail "helper did not refuse a command without the canonical marker (status=$refusal_status output=$refusal)"

set +e
wrong_command=$(env "$MARKER=$MARKER_VALUE" "$WRAPPER" /bin/true 2>&1)
wrong_command_status=$?
set -e
[[ $wrong_command_status == 2 && $wrong_command == *'restricted to the final test-harness run'* ]] ||
    fail "wrapper routed a marked non-harness command (status=$wrong_command_status output=$wrong_command)"

mkdir -p "$scratch/target/debug" "$scratch/artifacts"
cat >"$scratch/target/debug/test-harness" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ ${1:-} == run ]] || exit 71
expect=${2:-}
[[ -z ${HERMIT_VALIDATE_PRIVATE_TEST_ROOT+x} ]] || exit 72
[[ -x ${HERMIT_BIN:?} ]] || exit 73
[[ $PWD == "${EXPECTED_CWD:?}" ]] || exit 74
[[ $(id -u) == "${EXPECTED_UID:?}" && $(id -g) == "${EXPECTED_GID:?}" ]] || exit 75
[[ $(readlink /proc/self/ns/pid) == "${EXPECTED_PID_NS:?}" ]] || exit 76
[[ $(readlink /proc/self/ns/cgroup) == "${EXPECTED_CGROUP_NS:?}" ]] || exit 77
[[ -c /dev/null && -r /proc/self/status && -d /sys/fs/cgroup ]] || exit 78
case "$expect" in
    expect-host)
        [[ ! -e /test ]] || exit 79
        printf 'host-path\n'
        ;;
    expect-private)
        [[ -d /test && $(stat -c %a /test) == 1777 ]] || exit 80
        [[ -z $(find /test -mindepth 1 -print -quit) ]] || exit 81
        [[ $(readlink /proc/self/ns/mnt) != "${EXPECTED_MOUNT_NS:?}" ]] || exit 82
        read -r test_target test_fstype < <(findmnt -rn -T /test -o TARGET,FSTYPE)
        [[ $test_target == /test && $test_fstype == tmpfs ]] || exit 84
        printf 'private-path\n'
        ;;
    *) exit 83 ;;
esac
EOF
chmod +x "$scratch/target/debug/test-harness"

fake_hermit=$scratch/fake-hermit
printf '#!/usr/bin/env sh\nexit 0\n' >"$fake_hermit"
chmod +x "$fake_hermit"
binary_hash=$(sha256sum "$fake_hermit" | cut -d' ' -f1)
identity=$(printf '%s\n%s\n%s\n' binary-only "$binary_hash" none | sha256sum | cut -d' ' -f1)
bundle=$scratch/artifacts/$identity
mkdir "$bundle"
cp "$fake_hermit" "$bundle/hermit"
printf '%s\n' binary-only >"$bundle/kind"
printf '%s\n' "$binary_hash" >"$bundle/hermit.sha256"
printf '%s\n' "$bundle" >"$scratch/artifact.path"

export EXPECTED_CWD=$scratch
EXPECTED_UID=$(id -u)
export EXPECTED_UID
EXPECTED_GID=$(id -g)
export EXPECTED_GID
EXPECTED_PID_NS=$(readlink /proc/self/ns/pid)
export EXPECTED_PID_NS
EXPECTED_CGROUP_NS=$(readlink /proc/self/ns/cgroup)
export EXPECTED_CGROUP_NS
EXPECTED_MOUNT_NS=$(readlink /proc/self/ns/mnt)
export EXPECTED_MOUNT_NS
export HERMIT_E2E_ARTIFACT_POINTER=$scratch/artifact.path

host_route=$(cd "$scratch" && env -u "$MARKER" "$WRAPPER" target/debug/test-harness run expect-host)
[[ $host_route == host-path ]] || fail "unmarked wrapper did not retain the host path"

set +e
invalid_marker=$(
    cd "$scratch" && env "$MARKER=not-canonical" \
        "$WRAPPER" target/debug/test-harness run expect-private 2>&1
)
invalid_status=$?
set -e
[[ $invalid_status == 2 && $invalid_marker == *'invalid private-/test marker'* ]] ||
    fail "wrapper accepted an invalid marker (status=$invalid_status output=$invalid_marker)"

private_route=$(
    cd "$scratch" && env "$MARKER=$MARKER_VALUE" \
        "$WRAPPER" target/debug/test-harness run expect-private
)
[[ $private_route == private-path ]] || fail "marked wrapper did not enter the private root"

pwd_output=$(env "$MARKER=$MARKER_VALUE" "$HELPER" -- \
    "$HERMIT_UNDER_TEST" run --strict --base-env=minimal --workdir=/test -- /bin/pwd)
[[ $pwd_output == /test ]] || fail "Hermit reported an unexpected guest cwd: $pwd_output"

readonly regression=minimal_env_private_workdir_is_empty_for_both_verify_runs
grep -Fq "fn $regression" "$REGRESSION_ROOT/hermit-cli/tests/cli.rs" ||
    fail "the required Hermit regression is absent: $regression"
regression_output=$(env "$MARKER=$MARKER_VALUE" "$HELPER" -- \
    cargo test --manifest-path "$REGRESSION_ROOT/Cargo.toml" \
        -p hermit --test cli "$regression" -- --exact --nocapture 2>&1)
[[ $regression_output == *"test $regression ... ok"* && \
   $regression_output == *'1 passed; 0 failed'* ]] ||
    fail "the exact Hermit regression did not execute and pass: $regression_output"

[[ ! -e /test ]] || fail "host /test exists after the private run"
printf 'PRIVATE-TEST-ROOT refusal=PASS routing=PASS namespaces=PASS marker-leak=PASS hermit-pwd=%s regression=%s\n' \
    "$pwd_output" "$regression"
