#!/usr/bin/env bash
# READ-ONLY probe for the self-hosted runner box. Answers four questions that
# cannot be answered from the other validate host.
#
# WHAT IT DOES: lists containers, reads their config, and runs `exec` inside them
# to look at /dev/kvm, the PMU settings and network reachability.
# WHAT IT DOES NOT DO: it does not register, deregister, start, stop, restart or
# modify any runner or container; it pulls no images; and it CREATES NO FILES
# AND DELETES NOTHING -- there is no `rm`, no redirect to a file, and no temp
# directory anywhere in it. Every container command is `ps`, `inspect`, or
# `exec` of a read-only shell command; there are no run/pull/rm/stop/start
# calls. Two things do execute: `curl` (to /dev/null) and, where perf exists,
# `perf stat -e branches true`. Both are reads, but as root they run as root
# inside the container, so they are named here rather than glossed.
#
# SAFE TO RUN AS ROOT, and you probably must -- see the rootless warning it
# prints if it cannot see root-owned containers.
#
#   usage:  ./fedora-runner-probe.sh          (run as whoever can talk to the runtime;
#                                              if the containers are root-owned that
#                                              probably means sudo)

set -uo pipefail

# There is deliberately NO temp directory and NO `rm` anywhere in this file.
# An earlier draft created one with `mktemp -d` and removed it on exit, and
# nothing ever wrote to it. That is the shape of a known near-miss on this
# project -- an `rm -rf` on a path built from a variable that could come back
# empty -- so it is gone rather than guarded. The only occurrences of that
# verb left in this file are in this paragraph.

say()  { printf '\n%s\n' "$*"; }
line() { printf '  %s\n' "$*"; }

# ---------------------------------------------------------------- runtime ----
# Do not assume podman. Prefer whichever one actually has containers.
RT=""
for candidate in podman docker; do
    command -v "$candidate" >/dev/null 2>&1 || continue
    if "$candidate" ps --format '{{.Names}}' >/dev/null 2>&1; then RT=$candidate; break; fi
done
if [[ -z "$RT" ]]; then
    echo "FAIL: neither podman nor docker is usable here." >&2
    echo "  Both were tried. If the containers are root-owned, re-run with sudo." >&2
    exit 2
fi

echo "================ runner probe ================"
line "container runtime: $RT ($($RT --version 2>/dev/null | head -1))"
line "host: $(hostname)  kernel: $(uname -r)  as: $(id -un)"

# ROOTLESS BLINDNESS. A rootless podman sees ONLY this user's containers and
# gives no hint that root-owned ones exist. Measured: run as an ordinary user on
# a box whose hermit runners run as root, this script found five unrelated
# user-owned containers and reported on them confidently. A probe that presents
# a partial picture as the whole is worse than one that finds nothing, so say so
# loudly and unconditionally rather than only when the list is empty.
ROOTLESS="unknown"
if [[ $(id -u) -eq 0 ]]; then
    ROOTLESS="no (running as root)"
elif [[ $RT == podman ]]; then
    case "$(podman info --format '{{.Host.Security.Rootless}}' 2>/dev/null)" in
        true)  ROOTLESS="YES" ;;
        false) ROOTLESS="no (rootful podman as non-root)" ;;
        *)     ROOTLESS="probably (non-root user, could not query podman)" ;;
    esac
else
    ROOTLESS="unknown (docker client as non-root; the daemon may still be root)"
fi
line "rootless: $ROOTLESS"

mapfile -t ALL < <($RT ps --format '{{.Names}}' 2>/dev/null)
line "containers visible to this user: ${#ALL[@]}${ALL:+ -- ${ALL[*]}}"
if [[ $ROOTLESS == YES* || $ROOTLESS == probably* ]]; then
    echo
    echo "  ****************************************************************"
    echo "  * WARNING: this is a ROOTLESS container runtime.                *"
    echo "  * Root-owned containers are INVISIBLE to it. If the runners run *"
    echo "  * as root -- and hermit's do -- they will NOT appear below, and *"
    echo "  * any containers that DO appear are probably not the runners.   *"
    echo "  * RE-RUN WITH sudo BEFORE BELIEVING ANY ANSWER HERE.            *"
    echo "  ****************************************************************"
fi
if [[ ${#ALL[@]} -eq 0 ]]; then
    line "no running containers visible to this user"
fi

# Identify which containers are actually running a GitHub Actions runner.
RUNNERS=()
for c in "${ALL[@]:-}"; do
    [[ -n "$c" ]] || continue
    if $RT exec "$c" sh -c 'ps -eo args 2>/dev/null | grep -q "[R]unner.Listener"' 2>/dev/null; then
        RUNNERS+=("$c")
    fi
done

# ---------------------------------------------- Q1: containerized, as root? --
say "Q1. Are the runners containerized, and running as root?"
if [[ ${#RUNNERS[@]} -eq 0 ]]; then
    line "ANSWER: no running container has a Runner.Listener process."
    line "Either the runners are not containerized here, or they are not visible to $(id -un)."
    line "Runner.Listener processes on the HOST (outside any container):"
    # shellcheck disable=SC2009  # ps, not pgrep: the report wants the USER column too.
    ps -eo pid,user,args 2>/dev/null | grep "[R]unner.Listener" | sed 's/^/    /' || line "    none"
else
    line "ANSWER: yes -- ${#RUNNERS[@]} container(s) are running a runner."
    for c in "${RUNNERS[@]}"; do
        # shellcheck disable=SC2016  # single quotes are deliberate: $(pgrep ...) must
        # expand INSIDE the container, not on the host running this script.
        u=$($RT exec "$c" sh -c 'ps -o user= -p $(pgrep -f "[R]unner.Listener" | head -1) 2>/dev/null' 2>/dev/null | tr -d ' ')
        uid=$($RT exec "$c" id -u 2>/dev/null | tr -d ' ')
        priv=$($RT inspect --format '{{.HostConfig.Privileged}}' "$c" 2>/dev/null)
        line "$c: runner process user=${u:-?} (exec uid=${uid:-?})  privileged=${priv:-?}"
    done
fi

# ------------------------------------------------------- Q2: which image? ----
say "Q2. What image do they run? (could it be replaced by the pinned one?)"
if [[ ${#RUNNERS[@]} -eq 0 ]]; then
    line "not applicable -- no runner containers found"
else
    for c in "${RUNNERS[@]}"; do
        img=$($RT inspect --format '{{.Config.Image}}' "$c" 2>/dev/null)
        iid=$($RT inspect --format '{{.Image}}' "$c" 2>/dev/null)
        line "$c: image=${img:-?}"
        line "    image id/digest=${iid:-?}"
    done
    line "Replaceable if the runner is launched from a compose/systemd unit naming this image;"
    line "the launch definition is what would change, not anything inside the container."
fi

# --------------------------------------------------- Q3: ghcr reachability ---
# 401 is the CORRECT response from a live registry to an anonymous request.
# 000 means the request never completed, i.e. unreachable.
say "Q3. Can the runners reach ghcr.io? (401 = reachable, 000 = unreachable)"
probe_ghcr() {  # $1 = label, rest = command prefix
    local label=$1; shift
    local code
    code=$("$@" curl -s -o /dev/null -w '%{http_code}' --max-time 20 https://ghcr.io/v2/ 2>/dev/null)
    line "$label: ${code:-000} $( [[ ${code:-000} == 401 ]] && echo '(reachable)' || echo '(NOT reachable)')"
}
line "from the fedora HOST:"
probe_ghcr "  direct        " env
if command -v with-proxy >/dev/null 2>&1; then
    probe_ghcr "  via with-proxy" with-proxy
else
    line "  via with-proxy: with-proxy not present on this host"
fi
if [[ ${#RUNNERS[@]} -gt 0 ]]; then
    for c in "${RUNNERS[@]}"; do
        line "from INSIDE $c:"
        code=$($RT exec "$c" sh -c 'command -v curl >/dev/null 2>&1 && curl -s -o /dev/null -w "%{http_code}" --max-time 20 https://ghcr.io/v2/ || echo NOCURL' 2>/dev/null)
        line "  direct        : ${code:-000} $( [[ ${code:-000} == 401 ]] && echo '(reachable)' || echo '(NOT reachable)')"
        code=$($RT exec "$c" sh -c 'command -v with-proxy >/dev/null 2>&1 && with-proxy curl -s -o /dev/null -w "%{http_code}" --max-time 20 https://ghcr.io/v2/ || echo NOPROXYWRAPPER' 2>/dev/null)
        line "  via with-proxy: ${code:-000}"
    done
fi

# ------------------------------------------------ Q4: /dev/kvm and the PMU ---
# Deliberately checked from INSIDE the container. What the fedora host can see
# says nothing about what the container is given.
say "Q4. Do the runners have /dev/kvm and PMU access FROM INSIDE the container?"
if [[ ${#RUNNERS[@]} -eq 0 ]]; then
    line "not applicable -- no runner containers found; host values for reference:"
    line "  /dev/kvm on host: $(ls -l /dev/kvm 2>/dev/null || echo absent)"
    line "  perf_event_paranoid: $(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo '?')"
else
    for c in "${RUNNERS[@]}"; do
        line "$c:"
        line "  /dev/kvm: $($RT exec "$c" sh -c 'ls -l /dev/kvm 2>/dev/null || echo ABSENT' 2>/dev/null)"
        line "  /dev/kvm read+write: $($RT exec "$c" sh -c '[ -r /dev/kvm ] && [ -w /dev/kvm ] && echo YES || echo NO' 2>/dev/null)"
        line "  perf_event_paranoid: $($RT exec "$c" sh -c 'cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo unreadable' 2>/dev/null)"
        line "  capabilities (CapEff): $($RT exec "$c" sh -c 'grep CapEff /proc/self/status 2>/dev/null' 2>/dev/null)"
        # A real PMU test beats reading preconditions, when perf happens to exist.
        pmu=$($RT exec "$c" sh -c 'command -v perf >/dev/null 2>&1 && (perf stat -e branches true >/dev/null 2>&1 && echo COUNTED || echo REFUSED) || echo "perf not installed"' 2>/dev/null)
        line "  perf stat -e branches: ${pmu:-?}"
    done
    line "NOTE: paranoid <= 1 or CAP_PERFMON is what hermit's counted-branch clock needs."
    line "A 'perf not installed' line means the preconditions above are all you have."
fi

say "================ end of probe ================"
line "Nothing was modified. No images pulled, no containers started or stopped."
