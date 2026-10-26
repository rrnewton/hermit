# Private-memory observer disconnect controls

These two opt-in integration tests exercise a real client thread calling
`dr_invoke_syscall_as_app` while an external controller holds its actual private
memory observer. The close control requires completed withdrawal before the
process-exit callback destroys client state. The suspension control requires an
unrelated `dr_suspend_all_other_threads_ex` operation to reject that held observer
as a safe suspension point. The controller releases the observer independently;
neither test relies on a process-exit callback to release a correct close.

The production SDK contains no diagnostic output. `sdk-witness.patch` builds a
private test SDK that writes fixed records only at an actual contended close
acquisition and an actual client-thread safe-point decision. The supervisor
checks the closing thread or the held observer's thread ID. Missing witnesses,
unexpected statuses, and the 20-second timeout fail the test. The patch does not
change lock acquisition or safe-point decisions.

Run the following from a clean, disposable checkout of the tested revision.
Use the same existing Cargo target for both builds; retain the producer output
outside the checkout. This recipe needs CMake, a C compiler, `patch`, and `jq`.
It is explicit supplemental coverage, not part of default `cargo test` or an
automatic full-validation node.

```bash
set -euo pipefail
: "${CARGO_TARGET_DIR:?set the existing build target}"
: "${OBSERVER_EVIDENCE_DIR:?set an owned output directory outside the checkout}"
test -z "$(git status --porcelain)"
mkdir -p "$OBSERVER_EVIDENCE_DIR"
observer_patch="$PWD/reverie-dbt/tests/fixtures/private_memory_disconnect/sdk-witness.patch"
observer_sdk_source="$PWD/reverie-dbt/vendor/dynamorio"
patch --dry-run -d "$observer_sdk_source" -p1 < "$observer_patch"
patch -d "$observer_sdk_source" -p1 < "$observer_patch"
trap 'patch -R -d "$observer_sdk_source" -p1 < "$observer_patch"' EXIT
CARGO_INCREMENTAL=0 cargo test --offline --locked -j8 -p reverie-dbt \
  --features prototype-runtime --test private_memory_live --no-run \
  --message-format=json > "$OBSERVER_EVIDENCE_DIR/private-sdk-producer.jsonl"
observer_sdk=$(jq -r -s '
  [ .[] | select(.reason == "build-script-executed") | .env[]? |
    select(.[0] == "REVERIE_DBT_DYNAMORIO_HOME") | .[1] ] | unique |
  if length == 1 then .[0] else error("expected one emitted SDK path") end
' "$OBSERVER_EVIDENCE_DIR/private-sdk-producer.jsonl")
test -x "$observer_sdk/bin64/drrun"
patch -R -d "$observer_sdk_source" -p1 < "$observer_patch"
trap - EXIT
test -z "$(git status --porcelain)"

# Restore the ordinary production SDK before compiling/running the test binary.
# The private controls choose their separate SDK explicitly, not through an
# environment override of DbtRunner's compiled bundled SDK path.
CARGO_INCREMENTAL=0 REVERIE_DBT_PRIVATE_OBSERVER_WITNESS_SDK="$observer_sdk" \
  cargo test --offline --locked -j8 -p reverie-dbt --features prototype-runtime \
  --test private_memory_live private_memory_observer_disconnect_ \
  -- --ignored --nocapture --test-threads=1

# Required ordinary production-SDK counterpart: real post-init exit101 and
# normal exit0, one final retirement, and no callback after client exit.
CARGO_INCREMENTAL=0 cargo test --offline --locked -j8 -p reverie-dbt \
  --features prototype-runtime --test private_memory_live \
  private_memory_observer_is_withdrawn_before_client_shutdown \
  -- --exact --ignored --nocapture --test-threads=1
```

Retain source/patch hashes, emitted SDK provenance and install manifest, exact
test executable and selected SDK paths, command/status/streams, and the shared
record for each run. Set `REVERIE_DBT_PRIVATE_MEMORY_DISCONNECT_ARTIFACT_DIR` to
a distinct nonexistent output directory when retaining a single exact test.
Without that variable, the test's temporary fixture is removed after completion.

`API_RETURNED` is an observation, not an assertion: shutdown may terminate a
client worker after its observer returns but before the surrounding API returns.
Successful Linux x86_64 SDK termination and normal worker return both use
`SYS_exit(0)`; the supervisor reaps and checks those statuses. External kills,
fatal signals, and unsuccessful suspension are not accepted as successful
teardown. The required ordering assertions concern observer completion, actual
close contention, state destruction, and absence of later callbacks.
