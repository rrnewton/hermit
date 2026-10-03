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
extra=()
# The compat bucket node runs with --diagnostic-results; so does its Buck cell.
[[ $TEST == compat/* ]] && extra+=(--diagnostic-results)
out=$W/out
case $container in
"")
    env "${workdir_env[@]}" VALIDATE_RUN_STATE="$W/run-state" \
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
    # The bundle is mounted read-only at /src/bundle through hard links in the scratch
    # (a copy where links cannot cross filesystems), with the mountpoints the wrapper
    # adds under /src created first, so nothing in buck-out is written.
    R=$W/root
    mkdir -p "$R/target" "$R/agent-utils/rs/target" "$R/agent-utils/rs/.agent-utils-locks" \
        "$R/agent-utils/rs/.agent-utils-snapshots" "$W/results/buck-cell-out" "$W/pinned" ||
        emit_fatal "cannot create the pinned-root scratch tree"
    cp -al "$B" "$R/bundle" 2>/dev/null || { rm -rf "${R:?}/bundle"; cp -a --reflink=auto "$B" "$R/bundle"; } ||
        emit_fatal "cannot place the bundle in the pinned-root scratch tree"
    # Outputs go through the wrapper's /results mount (E2E_RESULT_ROOT).
    out=$W/results/buck-cell-out
    o=/results/buck-cell-out
    env HERMIT_E2E_EMPTY_WORKDIR=/test VALIDATE_RUN_STATE="$W/run-state" \
        E2E_RESULT_ROOT="$W/results" E2E_RUN_ID="$run_id" \
        E2E_KEEP_VERIFY_LOGS=1 E2E_PARITY_POST_PASS=0 \
        timeout --kill-after=10 $((deadline + 60)) \
        "$wrapper" --src "$R" --out "$W/pinned" \
        --env HERMIT_E2E_EMPTY_WORKDIR --env VALIDATE_RUN_STATE --env E2E_RESULT_ROOT \
        --env E2E_RUN_ID --env E2E_KEEP_VERIFY_LOGS --env E2E_PARITY_POST_PASS -- \
        env E2E_BUILD_ROOT=/src/bundle/build \
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

# Evidence policy: a passing verify cell must return its verdict JSON and both DETLOGs.
outcome=$(python3 -c 'import json,sys
rows=[json.loads(l) for l in open(sys.argv[1]) if l.strip()]
print(rows[-1]["outcome"] if rows else "")' "$out/results.jsonl" 2>/dev/null)
n_detlogs=$(find "$A" -maxdepth 1 -name 'cell__verify-logs__*run[12]_log_*' | wc -l)
evidence_complete=true
missing=()
[[ -s $out/tpx.jsonl ]] || { evidence_complete=false; missing+=(tpx.jsonl); }
if [[ $outcome == PASS && $MODE == verify ]]; then
    [[ -s $A/cell__verify-1.json ]] || { evidence_complete=false; missing+=(verify-1.json); }
    ((n_detlogs >= 2)) || { evidence_complete=false; missing+=(detlogs); }
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
