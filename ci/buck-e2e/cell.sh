#!/bin/bash
# Run ONE hermit e2e manifest cell through test-harness and return its evidence as
# flat Tpx test-result artifacts (Tpx collects top-level files only).
#
# usage: cell.sh TEST MODE BACKEND [extra test-harness run flags...]
# env:   HERMIT_E2E_BUNDLE         bundle dir (hermit/, bin/, src/, build/, run-state/,
#                                  SOURCE_SHA): the :bundle target, or a prebuilt bundle
#        HERMIT_E2E_BUNDLE_SHA256  optional: BUNDLE.sha256 a prebuilt bundle must match
#        HERMIT_E2E_ROUTE[_REASON] where the generator routed this cell, and why
#        HERMIT_E2E_CONTAINER[_REASON]
#                                  "pinned-root": run the harness inside the bundle
#                                  source's ci/hermetic/run-in-pinned-root.sh, the
#                                  privileged podman container the cargo flow runs every
#                                  e2e node in (local cells only); empty: on this host
#        CELL_DEADLINE_S           optional cap on the harness wall time
#                                  (default: Tpx timeout - 30 s; Tpx returns no
#                                  artifacts at all from an RE action it kills)
# stdout: Tpx HPHP-JSON (`type = "json"`): the harness's test_done for the cell, turned
#         into a failure if its evidence is incomplete, then all_done. Always exits 0.
set -u
TEST=$1 MODE=$2 BACKEND=$3
shift 3
CELL="$TEST/$MODE@$BACKEND"
t0=$(date +%s%N)

emit_fatal() {
    python3 -c 'import json,sys; print(json.dumps({"op":"test_done","test":sys.argv[1],"status":"failed","details":json.dumps({"outcome":"ERROR","reason":sys.argv[2]})})); print(json.dumps({"op":"all_done"}))' "$CELL" "$1"
    exit 0
}

[[ -n ${TEST_RESULT_ARTIFACTS_DIR:-} && -n ${TEST_RESULT_ARTIFACT_ANNOTATIONS_DIR:-} ]] ||
    emit_fatal "Tpx artifact dirs are not set; the target needs the tpx-enable-artifact-reporting label"
mkdir -p "$TEST_RESULT_ARTIFACTS_DIR" "$TEST_RESULT_ARTIFACT_ANNOTATIONS_DIR"
# On RE these are relative to /re_cwd; absolutize before anything changes directory.
A=$(realpath "$TEST_RESULT_ARTIFACTS_DIR")
N=$(realpath "$TEST_RESULT_ARTIFACT_ANNOTATIONS_DIR")
run_id="buck-$(hostname -s)-$$-$(date +%s%N)"
# Named by the run id, so the testx listing shows which run an execution was
# without fetching anything (ingest.py ties local artifact copies to it).
printf '%s\n' "$run_id" >"$A/run_id.$run_id"
B=$(realpath "${HERMIT_E2E_BUNDLE:?}")
bundle_sha=$(cat "$B/BUNDLE.sha256" 2>/dev/null || true)
if [[ -n ${HERMIT_E2E_BUNDLE_SHA256:-} && $bundle_sha != "$HERMIT_E2E_BUNDLE_SHA256" ]]; then
    emit_fatal "bundle $B is ${bundle_sha:-unreadable}, but the targets were generated from $HERMIT_E2E_BUNDLE_SHA256"
fi
if [[ -f $B/SOURCE_SHA ]]; then
    SHA=$(cat "$B/SOURCE_SHA")
else
    SHA=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["source_sha"])' "$B/PROVENANCE.json") ||
        emit_fatal "bundle has neither SOURCE_SHA nor a readable PROVENANCE.json"
fi

# Scratch next to the artifacts dir: not a declared output (never uploaded), and not
# under /tmp, which hermit hides from guests with a private tmpfs.
W=$(mktemp -d "$(dirname "$A")/hermit-cell.XXXXXX") || emit_fatal "cannot create a scratch directory"
mkdir -p "$W/run-state"
cp -a "$B/run-state/." "$W/run-state/" || emit_fatal "cannot copy the compat run state"
slug=$(printf '%s' "$TEST" | tr / -)-$MODE-$BACKEND
tpx_timeout=${TPX_TIMEOUT_SEC:-600}
deadline=${CELL_DEADLINE_S:-$((tpx_timeout - 30))}
((deadline > 10)) || deadline=10

# The cargo flow runs every cell in a fresh tmpfs at /test (HERMIT_E2E_EMPTY_WORKDIR).
# hermit can only mount it where /test exists: a local host can provide it, an RE
# worker cannot, so RE cells use the harness's default bound /tmp/test workdir.
# The pinned-root container always has a fresh tmpfs at /test.
container=${HERMIT_E2E_CONTAINER:-}
workdir_env=()
[[ ${HERMIT_E2E_ROUTE:-} == local && (-d /test || $container == pinned-root) ]] &&
    workdir_env=(HERMIT_E2E_EMPTY_WORKDIR=/test)
# The harness measures each budgeted invocation's live CPU from a cgroup of its own and
# refuses the invocation when it cannot create one. An RE worker runs the test inside a
# root-owned cgroup that cell.sh cannot write (cgroup.procs: EACCES), so an RE cell is a
# run without cgroups on purpose and sets HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN=1. The
# harness still tries a cgroup first and only then uses the agent-utils process-group
# scan, whose rows name that source. A local cell runs in a cgroup it can use, so it
# never inherits a caller's marker; neither does the pinned-root container.
if [[ ${HERMIT_E2E_ROUTE:-} == re ]]; then
    cpu_scan_env=(HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN=1)
else
    cpu_scan_env=(-u HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN)
fi
extra=()
# The compat bucket node runs with --diagnostic-results; so does its Buck cell.
[[ $TEST == compat/* ]] && extra+=(--diagnostic-results)
out=$W/out
case $container in
"")
    env "${cpu_scan_env[@]}" "${workdir_env[@]}" VALIDATE_RUN_STATE="$W/run-state" \
        E2E_RESULT_ROOT="$W/results" E2E_BUILD_ROOT="$B/build" E2E_RUN_ID="$run_id" \
        E2E_KEEP_VERIFY_LOGS=1 E2E_PARITY_POST_PASS=0 \
        HERMIT_BIN="$B/hermit/hermit" HERMIT_INSTALL_DIR="$B/hermit/install" \
        timeout --kill-after=10 "$deadline" \
        "$B/bin/test-harness" run --repo-root "$B/src" --source-sha "$SHA" \
        --test "$TEST" --mode "$MODE" --backend "$BACKEND" --prebuilt --no-retry \
        --results "$out/results.jsonl" --junit "$out/junit.xml" --tpx-json "$out/tpx.jsonl" \
        "${extra[@]}" "$@" >"$W/harness.stdout" 2>"$W/harness.stderr"
    harness_rc=$?
    ;;
pinned-root)
    # Cells that need CAP_SYS_ADMIN (DBT's /test mount namespace), the privileged lane or
    # the container's identity (hostname, /results). The wrapper and the image digest it
    # pins come from the bundle's own source snapshot, as in the cargo flow.
    [[ ${HERMIT_E2E_ROUTE:-} == local ]] ||
        emit_fatal "HERMIT_E2E_CONTAINER=pinned-root needs a local route, not '${HERMIT_E2E_ROUTE:-}'"
    wrapper=$B/src/ci/hermetic/run-in-pinned-root.sh
    [[ -x $wrapper ]] || emit_fatal "the bundle source has no executable ci/hermetic/run-in-pinned-root.sh"
    "$wrapper" --check-image >"$W/check-image.log" 2>&1 ||
        emit_fatal "pinned-root image unavailable: $(tr '\n' ' ' <"$W/check-image.log")(build it with ci/hermetic/build-image.sh)"
    # /src is mounted read-write, as in the cargo flow (--src-rw): hermit writes its
    # private verify and engagement summaries into its working directory, the bundle's
    # src/, when no enclosing git checkout ignores `ignored/`. /src/bundle is therefore
    # a private copy (a reflink on btrfs, about 1 s), never hard links, so nothing in
    # buck-out is written. The mountpoints the wrapper adds under /src are created first.
    R=$W/root
    mkdir -p "$R/target" "$R/agent-utils/rs/target" "$R/agent-utils/rs/.agent-utils-locks" \
        "$R/agent-utils/rs/.agent-utils-snapshots" "$W/results/buck-cell-out" "$W/pinned" ||
        emit_fatal "cannot create the pinned-root scratch tree"
    cp -a --reflink=auto "$B" "$R/bundle" ||
        emit_fatal "cannot place the bundle in the pinned-root scratch tree"
    # DynamoRIO's private loader loads the DBT client's own libc, libm and libgcc_s. It
    # searches the client's RPATH and directory, LD_LIBRARY_PATH and /lib, /lib64,
    # /usr/lib64..., not the paths the image's nix loader knows, so in the container it
    # finds only ld.so and the guest dies before the client connects ("DBT evidence
    # received no image START"). The cargo flow builds the client in the container and
    # does not hit this. So, in the private copy only, the client's directory gets a link
    # to every library the image's own loader resolves for the client outside it,
    # interpreter included: libc and ld.so come from one glibc, and a library the bundle
    # ships is never replaced. The staged bundle and RE cells are unchanged. Only a DBT
    # cell loads the client, so only a DBT cell gets the links.
    link_dbt_runtime='set -eu
r=$1
shift
link() { [ -e "$r/$2" ] || [ -L "$r/$2" ] || ln -s "$1" "$r/$2"; }
deps=$(ldd "$r/libreverie_dbt_client.so" "$r/libdetcore_dbt.so")
printf "%s\n" "$deps" | while read -r name arrow path rest; do
    case "$arrow $path" in
    "=> not"*) echo "cell.sh: the pinned-root image has no $name, which the DBT client needs" >&2; exit 125 ;;
    "=> $r/"*) ;;
    "=> /"*) link "$path" "${name##*/}" ;;
    "("*) case $name in /*) link "$name" "${name##*/}" ;; esac ;;
    esac
done
exec "$@"'
    prologue=()
    [[ $BACKEND != dbt ]] ||
        prologue=(sh -c "$link_dbt_runtime" link-dbt-runtime /src/bundle/hermit/install/rsrcs)
    # Outputs go through the wrapper's /results mount (E2E_RESULT_ROOT).
    out=$W/results/buck-cell-out
    o=/results/buck-cell-out
    # Tpx kills the cell at deadline + 30 s. The wrapper gets what is left of the deadline
    # after the copy above, and the harness that less 15 s for starting and removing the
    # container, so a hung container is stopped here, with its artifacts, before Tpx.
    outer=$((deadline - ($(date +%s%N) - t0) / 1000000000))
    ((outer > 25)) || outer=25
    deadline=$((outer - 15))
    env "${cpu_scan_env[@]}" HERMIT_E2E_EMPTY_WORKDIR=/test VALIDATE_RUN_STATE="$W/run-state" \
        E2E_RESULT_ROOT="$W/results" E2E_RUN_ID="$run_id" \
        E2E_KEEP_VERIFY_LOGS=1 E2E_PARITY_POST_PASS=0 \
        timeout --kill-after=10 "$outer" \
        "$wrapper" --src "$R" --out "$W/pinned" --src-rw \
        --env HERMIT_E2E_EMPTY_WORKDIR --env VALIDATE_RUN_STATE --env E2E_RESULT_ROOT \
        --env E2E_RUN_ID --env E2E_KEEP_VERIFY_LOGS --env E2E_PARITY_POST_PASS -- \
        "${prologue[@]}" env E2E_BUILD_ROOT=/src/bundle/build \
        HERMIT_BIN=/src/bundle/hermit/hermit HERMIT_INSTALL_DIR=/src/bundle/hermit/install \
        timeout --kill-after=10 "$deadline" \
        /src/bundle/bin/test-harness run --repo-root /src/bundle/src --source-sha "$SHA" \
        --test "$TEST" --mode "$MODE" --backend "$BACKEND" --prebuilt --no-retry \
        --results "$o/results.jsonl" --junit "$o/junit.xml" --tpx-json "$o/tpx.jsonl" \
        "${extra[@]}" "$@" >"$W/harness.stdout" 2>"$W/harness.stderr"
    harness_rc=$?
    ;;
*) emit_fatal "unknown HERMIT_E2E_CONTAINER '$container' (expected pinned-root or empty)" ;;
esac
t1=$(date +%s%N)

copy_errors=0
put() { # put SRC NAME: logs over 256 KiB are zstd'd
    local src=$1 name=$2
    if [[ $(stat -c %s "$src") -gt 262144 ]]; then
        zstd -q -3 "$src" -o "$A/$name.zst" || copy_errors=$((copy_errors + 1))
    else
        cp "$src" "$A/$name" || copy_errors=$((copy_errors + 1))
    fi
}
for f in harness.stdout harness.stderr; do put "$W/$f" "$f"; done
for f in results.jsonl junit.xml tpx.jsonl summary.json; do
    [[ -f $out/$f ]] && put "$out/$f" "$f"
done
celldir="$W/results/runs/$run_id/$slug"
if [[ -d $celldir ]]; then
    while IFS= read -r -d '' f; do
        rel=${f#"$celldir/"}
        case $rel in fixtures/* | home/* | xdg-config/* | tmp/* | workdir/* | recording/*) continue ;; esac
        put "$f" "cell__${rel//\//__}"
    done < <(find "$celldir" -type f -print0)
fi

# Evidence policy: a passing verify cell must return its verdict JSON and the DETLOGs
# hermit keeps: after a matched verify only run 1's log (the golden copy; hermit deletes
# run 2's), otherwise both.
outcome=$(python3 -c 'import json,sys
rows=[json.loads(l) for l in open(sys.argv[1]) if l.strip()]
print(rows[-1]["outcome"] if rows else "")' "$out/results.jsonl" 2>/dev/null)
n_run1=$(find "$A" -maxdepth 1 -name 'cell__verify-logs__*run1_log_*' | wc -l)
n_run2=$(find "$A" -maxdepth 1 -name 'cell__verify-logs__*run2_log_*' | wc -l)
n_detlogs=$((n_run1 + n_run2))
verdict=$(python3 -c 'import json,sys
print(json.load(open(sys.argv[1])).get("verdict") or "")' "$A/cell__verify-1.json" 2>/dev/null) || verdict=
evidence_complete=true
missing=()
[[ -s $out/tpx.jsonl ]] || { evidence_complete=false; missing+=(tpx.jsonl); }
if [[ $outcome == PASS && $MODE == verify ]]; then
    # hermit always writes a verdict; a file without one is not a verify report.
    [[ -s $A/cell__verify-1.json && -n $verdict ]] || { evidence_complete=false; missing+=(verify-1.json); }
    if [[ $verdict == matched ]]; then
        ((n_run1 == 1 && n_run2 == 0)) || { evidence_complete=false; missing+=(detlogs); }
    else
        ((n_run1 >= 1 && n_run2 >= 1)) || { evidence_complete=false; missing+=(detlogs); }
    fi
fi
((copy_errors == 0)) || { evidence_complete=false; missing+=("copy_errors=$copy_errors"); }

python3 - "$A" "$B" <<PY
import hashlib, json, os, sys
art, bundle = sys.argv[1:3]
json.dump({
    "schema": 1, "cell": "$CELL", "test": "$TEST", "mode": "$MODE", "backend": "$BACKEND",
    "route": "${HERMIT_E2E_ROUTE:-}", "route_reason": "${HERMIT_E2E_ROUTE_REASON:-}",
    "container": os.environ.get("HERMIT_E2E_CONTAINER", ""),
    "container_reason": os.environ.get("HERMIT_E2E_CONTAINER_REASON", ""),
    "empty_workdir": "${workdir_env[*]}",
    "harness_rc": $harness_rc, "outcome": "$outcome", "deadline_s": $deadline,
    "tpx_timeout_s": $tpx_timeout, "wall_ms": $(((t1 - t0) / 1000000)),
    "evidence_complete": "$evidence_complete" == "true", "missing": "${missing[*]}".split(),
    "detlogs": $n_detlogs, "host": "$(hostname)", "re_platform": "${RE_PLATFORM:-local}",
    "nproc": $(nproc), "run_id": "$run_id", "bundle_sha256": "$bundle_sha",
    "source_sha": "$SHA",
    "provenance": json.load(open(os.path.join(bundle, "PROVENANCE.json"))) if os.path.exists(os.path.join(bundle, "PROVENANCE.json")) else None,
    # sha256 of every other artifact as written here, so a fetch can be checked byte-exact.
    "artifact_sha256": {n: hashlib.sha256(open(os.path.join(art, n), "rb").read()).hexdigest()
                        for n in sorted(os.listdir(art)) if n != "result.json"},
}, open(os.path.join(art, "result.json"), "w"), indent=1, sort_keys=True)
PY
for f in result.json tpx.jsonl summary.json results.jsonl harness.stderr; do
    [[ -f $A/$f ]] && printf '{"type": {"generic_text_log": {}}, "description": "%s"}\n' "$f" >"$N/$f.annotation"
done
rm -rf "${W:?}"

python3 - "$A/tpx.jsonl" "$CELL" "$evidence_complete" "$harness_rc" "${missing[*]}" <<'PY'
import json, os, sys
path, cell, complete, rc, missing = sys.argv[1:6]
records = [json.loads(l) for l in open(path) if l.strip()] if os.path.exists(path) else []
done = [r for r in records if r.get("op") == "test_done"]
if len(done) != 1:
    done = [{"op": "test_done", "test": cell, "status": "failed",
             "details": json.dumps({"outcome": "ERROR", "reason": f"harness rc={rc} produced {len(done)} test_done records"})}]
elif complete != "true" and done[0]["status"] == "passed":
    details = json.loads(done[0]["details"])
    details.update(outcome="ERROR", reason=f"evidence incomplete: {missing}")
    done[0].update(status="failed", details=json.dumps(details))
print(json.dumps(done[0]))
print(json.dumps({"op": "all_done"}))
PY
exit 0
