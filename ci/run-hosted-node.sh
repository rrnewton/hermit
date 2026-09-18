#!/usr/bin/env bash
# Run a committed hosted validation node inside the user namespace that gives
# its nested per-physical-run mount namespace CAP_SYS_ADMIN. GitHub's runner
# account can create a user namespace, but cannot unshare CLONE_NEWNS directly;
# merely probing the combined user+mount operation in an earlier step does not
# carry that authority into this process.

set -euo pipefail

if [[ ${GITHUB_ACTIONS:-} != true ]]; then
    echo "run-hosted-node.sh: this wrapper is only for GitHub-hosted validation" >&2
    exit 2
fi

echo "HOSTED-ISOLATION: entering a per-job user/mount/PID/network namespace; local validate still uses its pinned-root and cgroup policy" >&2
exec unshare --user --map-root-user --pid --fork --uts --net --mount \
    bash -c '
        set -euo pipefail
        mount -t proc proc /proc
        mount -t sysfs sysfs /sys
        # Exercise the exact nested mount capability the per-physical-run /test
        # helper needs, without leaving the probe mount visible to validation.
        unshare --mount bash -c \
            "mount --make-rprivate / && mount -t tmpfs -o nosuid,nodev,mode=1777 tmpfs /test && umount /test"
        exec "$@"
    ' \
    bash ./ci/run-node.sh "$@"
