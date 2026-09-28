#!/usr/bin/env bash
# Run a command inside the nix-pinned validate root. The canonical host-side
# validate plan uses this wrapper for each build and test DAG node.
#
# The existing outer systemd-run, validate-lock, DAG scheduler, and cgroup
# policy stay on the host; this wrapper adds the pinned filesystem and network
# boundary without creating a second resource or cgroup layer. Each invocation
# is one privileged podman container pinned BY DIGEST, with /dev/kvm passed
# through, no runtime network, source at /src (read-only by default, explicitly
# writable for validation nodes), and separate writable output and target
# volumes.
#
# WHY BY DIGEST AND NOT BY TAG. A tag is mutable; a digest is the artifact that
# actually ran. The digest belongs in the receipt next to the flake.lock: the
# digest says what ran, the lock says how to rebuild it. See README.md.
#
#   usage: run-in-pinned-root.sh --src DIR --out DIR [--digest NAME@SHA]
#                                [--src-rw] [--cargo-home DIR] [--env NAME]...
#                                [--share-cgroup-parent] -- CMD...
#
# --src-rw mounts the source WRITABLE. The default is read-only and stays that
# way, but a test phase legitimately writes into its own tree (target/ci,
# ignored/e2e/build), exactly as a GitHub shard job writes into its checkout.
# Read-only is the right default for a one-shot command; it is not a property
# the test phase can satisfy.
#
# --cargo-home mounts an already-populated CARGO_HOME. The test phase has no
# network BY DESIGN, so cargo must find its registry and git database already
# present or it cannot even resolve the dependency graph. This is the local
# equivalent of the shard jobs' `Swatinem/rust-cache` restore, not a workaround:
# in both cases the cache is an input carried across the phase boundary.
#
# DAGRUN_TEST_COUNTS_PATH is scheduler-owned on the host. When requested through
# --env, its parent directory is mounted at /dagrun-test-counts and the child is
# given the translated path. This lets an in-container test producer atomically
# publish the same evidence file the host scheduler will read after podman exits.
#
# --share-cgroup-parent transfers a held ancestor directory to test-harness,
# which authenticates and closes it at startup. Use it only for that consumer;
# ordinary pinned commands receive no parent descriptor.

set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
DIGEST_FILE="$HERE/image.digest"

src=""; out=""; digest=""; src_mode="ro=true"; cargo_home=""
share_cgroup_parent=false
pass_env=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --src) src=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        --digest) digest=$2; shift 2 ;;
        --src-rw) src_mode="ro=false"; shift ;;
        --cargo-home) cargo_home=$2; shift 2 ;;
        --share-cgroup-parent) share_cgroup_parent=true; shift ;;
        --env) pass_env+=("$2"); shift 2 ;;
        --) shift; break ;;
        *) echo "run-in-pinned-root: unexpected argument '$1'" >&2; exit 2 ;;
    esac
done

[[ -n "$src" ]] || { echo "run-in-pinned-root: --src is required" >&2; exit 2; }
[[ -n "$out" ]] || { echo "run-in-pinned-root: --out is required" >&2; exit 2; }
[[ $# -gt 0 ]] || { echo "run-in-pinned-root: a command is required after --" >&2; exit 2; }

for name in DAGRUN_DELEGATED_PARENT_FD DAGRUN_DELEGATED_PARENT_CHILD; do
    if [[ -v $name ]]; then
        echo "run-in-pinned-root: inherited $name is not an owned parent handoff" >&2
        exit 2
    fi
    for requested in "${pass_env[@]}"; do
        if [[ $requested == "$name" ]]; then
            echo "run-in-pinned-root: $name is reserved for --share-cgroup-parent" >&2
            exit 2
        fi
    done
done

# Committed DAG commands are checkout-portable and therefore pass paths relative
# to the scheduler's repository working directory. Podman bind sources must be
# absolute; resolve them here at the execution boundary without rewriting the
# DAG in validate.
src=$(realpath -m -- "$src")
out=$(realpath -m -- "$out")
if [[ -n "$cargo_home" ]]; then
    cargo_home=$(realpath -m -- "$cargo_home")
fi

if [[ -z "$digest" ]]; then
    [[ -f "$DIGEST_FILE" ]] || {
        echo "run-in-pinned-root: no --digest and no $DIGEST_FILE." >&2
        echo "  Build the image first: ci/hermetic/build-image.sh" >&2
        exit 2
    }
    digest=$(tr -d '[:space:]' < "$DIGEST_FILE")
fi

# FAIL CLOSED ON A MISSING IMAGE. Falling back to a tag, or to the host, would
# silently produce a run that is not hermetic while still reporting success --
# which is worse than not running at all, because the receipt would claim a
# pinned root it did not use.
if ! podman image exists "$digest"; then
    echo "run-in-pinned-root: image $digest is not present locally." >&2
    echo "  This path does not fall back to a tag or to the host: a run that is" >&2
    echo "  not in the pinned root must not be recorded as if it were." >&2
    echo "  Rebuild it from the committed lock: ci/hermetic/build-image.sh" >&2
    exit 1
fi

mkdir -p "$out/target" "$out/home"

cargo_mount=(); cargo_home_in=/build/.cargo
git_mounts=()
if [[ -f "$src/.git" ]]; then
    git_common_dir=$(git -C "$src" rev-parse --path-format=absolute --git-common-dir)
    git_mounts+=(--mount "type=bind,source=$git_common_dir,destination=$git_common_dir,ro=true")

    # A linked worktree gives each initialized submodule a relative .git file.
    # Relocating the source to /src changes what that relative path means, so
    # reproduce both the resolved git-dir and its configured worktree path.
    while IFS= read -r submodule_path; do
        submodule_root="$src/$submodule_path"
        [[ -f "$submodule_root/.git" ]] || continue
        submodule_git_dir=$(git -C "$submodule_root" rev-parse --path-format=absolute --git-dir)
        raw_git_dir=$(sed -n "s/^gitdir: //p" "$submodule_root/.git")
        if [[ $raw_git_dir == /* ]]; then
            guest_git_dir=$raw_git_dir
        else
            guest_git_dir=$(realpath -m "/src/$submodule_path/$raw_git_dir")
        fi
        git_mounts+=(--mount "type=bind,source=$submodule_git_dir,destination=$guest_git_dir,ro=true")
        core_worktree=$(git -C "$submodule_root" config --local --get core.worktree || true)
        if [[ -n $core_worktree ]]; then
            guest_worktree=$(realpath -m "$guest_git_dir/$core_worktree")
            git_mounts+=(--mount "type=bind,source=$submodule_root,destination=$guest_worktree,$src_mode")
        fi
    done < <(git -C "$src" submodule foreach --quiet 'printf "%s\n" "$sm_path"')
fi
if [[ -n "$cargo_home" ]]; then
    [[ -d "$cargo_home" ]] || {
        echo "run-in-pinned-root: --cargo-home '$cargo_home' is not a directory." >&2
        echo "  It must be populated BEFORE this runs -- there is no network in here" >&2
        echo "  to populate it from. Run the build phase first." >&2
        exit 2
    }
    cargo_mount=(--mount "type=bind,source=$cargo_home,destination=/cargo")
    cargo_home_in=/cargo
fi

env_args=()
extra_mounts=()
device_args=()
if [[ -e /dev/kvm ]]; then
    device_args+=(--device /dev/kvm)
fi
for name in "${pass_env[@]}"; do
    [[ $name =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || {
        echo "run-in-pinned-root: invalid environment name '$name'." >&2
        exit 2
    }
    [[ -v $name ]] || continue
    case "$name" in
        E2E_RESULT_ROOT)
            mkdir -p "${!name}"
            extra_mounts+=(--mount "type=bind,source=${!name},destination=/results")
            env_args+=(-e E2E_RESULT_ROOT=/results)
            ;;
        E2E_BUILD_ROOT)
            env_args+=(-e E2E_BUILD_ROOT=/src/target/e2e-build)
            ;;
        DAGRUN_TEST_COUNTS_PATH)
            [[ ${!name} == /* ]] || {
                echo "run-in-pinned-root: DAGRUN_TEST_COUNTS_PATH must be absolute" >&2
                exit 2
            }
            counts_dir=$(dirname -- "${!name}")
            counts_file=$(basename -- "${!name}")
            mkdir -p "$counts_dir"
            extra_mounts+=(--mount "type=bind,source=$counts_dir,destination=/dagrun-test-counts")
            env_args+=(-e "DAGRUN_TEST_COUNTS_PATH=/dagrun-test-counts/$counts_file")
            ;;
        *) env_args+=(--env "$name") ;;
    esac
done

parent_fd_args=()
if $share_cgroup_parent; then
    refuse_parent() {
        echo "run-in-pinned-root: cannot share current cgroup parent: $*" >&2
        exit 2
    }
    read_current_membership() {
        local line
        current_membership=""
        while IFS= read -r line; do
            [[ $line == 0::* ]] || continue
            [[ -z $current_membership ]] || refuse_parent "multiple unified memberships"
            current_membership=${line#0::}
        done < /proc/self/cgroup
        [[ $current_membership == /* && $current_membership != / ]] ||
            refuse_parent "a nonroot host cgroup is required"
        case "$current_membership/" in
            *//*|*/./*|*/../*) refuse_parent "membership is not an absolute normal path" ;;
        esac
    }
    read_current_membership
    cgroup_membership=$current_membership
    cgroup_current="/sys/fs/cgroup$cgroup_membership"
    cgroup_child=${cgroup_membership##*/}
    cgroup_ancestor=${cgroup_current%/*}
    [[ -d $cgroup_current && ! -L $cgroup_current &&
       -d $cgroup_ancestor && ! -L $cgroup_ancestor ]] ||
        refuse_parent "current cgroup and ancestor must be directories"

    # Bash keeps this explicitly opened descriptor across exec. Podman passes
    # only this descriptor; the receiver resolves the child with openat2 and
    # verifies the actual current cgroup before retaining its own CLOEXEC fd.
    exec {cgroup_parent_fd}<"$cgroup_ancestor"
    cgroup_fd_path="/proc/$$/fd/$cgroup_parent_fd"
    [[ $(stat -L -f -c %t -- "$cgroup_fd_path") == 63677270 ]] ||
        refuse_parent "held ancestor is not on cgroup-v2"
    [[ $(stat -L -c '%d:%i' -- "$cgroup_fd_path") == \
       "$(stat -L -c '%d:%i' -- "$cgroup_ancestor")" ]] ||
        refuse_parent "ancestor pathname changed while opening it"
    [[ -f $cgroup_current/cgroup.procs && ! -L $cgroup_current/cgroup.procs ]] ||
        refuse_parent "current membership roster is not a regular control file"
    cgroup_self_present=false
    while IFS= read -r member; do
        [[ $member =~ ^[0-9]+$ ]] || refuse_parent "invalid current membership roster"
        [[ $member == "$$" ]] && cgroup_self_present=true
    done < "$cgroup_current/cgroup.procs"
    $cgroup_self_present || refuse_parent "wrapper is absent from current membership roster"
    read_current_membership
    [[ $current_membership == "$cgroup_membership" ]] ||
        refuse_parent "current membership changed during handoff"
    [[ $(stat -L -c '%d:%i' -- "$cgroup_fd_path") == \
       "$(stat -L -c '%d:%i' -- "$cgroup_ancestor")" ]] ||
        refuse_parent "ancestor pathname changed before handoff"
    parent_fd_args=(--preserve-fd "$cgroup_parent_fd")
    env_args+=(-e "DAGRUN_DELEGATED_PARENT_FD=$cgroup_parent_fd"
               -e "DAGRUN_DELEGATED_PARENT_CHILD=$cgroup_child")
fi

# `--network=none` is the point, not a precaution: if the run can reach the
# network it can pick up something the lock does not describe, and the rebuild
# guarantee is void. CARGO_NET_OFFLINE in the image makes that fail loudly.
exec podman run --rm \
    --cgroups=disabled \
    "${parent_fd_args[@]}" \
    --privileged \
    --hostname=hermetic-container.local \
    "${device_args[@]}" \
    --network=none \
    --http-proxy=false \
    --tmpfs /test:rw,nosuid,nodev,mode=1777 \
    --mount "type=bind,source=$src,destination=/src,$src_mode" \
    --mount "type=bind,source=$out/target,destination=/src/target" \
    --mount "type=bind,source=$out/home,destination=/build" \
    "${cargo_mount[@]}" \
    "${git_mounts[@]}" \
    "${extra_mounts[@]}" \
    "${env_args[@]}" \
    -e HOME=/build \
    -e CARGO_HOME="$cargo_home_in" \
    -e CARGO_TARGET_DIR=/src/target \
    -w /src \
    "$digest" \
    "$@"
