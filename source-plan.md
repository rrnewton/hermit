# Package D: shared DBT statistics capture

The selected next slice is one physical statistics capture owner in Reverie's existing launcher. The common verifier and controller-input provenance work are already coordinator-reported landed; this slice removes Hermit's duplicate TempDir owner, client-argument construction, file reading and aggregation while retaining its existing thin bind declaration. Hermit retains its existing strict execution-evidence policy. Implementation was authorized after the read-only plan and source/design review; Cargo remains executor-owned.

## Fixed source and landing observations

Use Hermit `55f75cbaf7db7b2715f639c14d09a508d9a78932` and its exact Reverie pin `e1ecaeb31a0bb1f8b86f36e769296ab6a7de3782`. The latter's parent is `6dd1d62bf71ae171945feed82154a43aedf555bf`.

The reported Hermit landing stack resolves locally to `2a6b3c87e25d8917fee58686fd56c57cfee81639`, `9f57cc36bf13e0484be50e141384be6f3bd9996d`, `ff95b21adbbe4b36f300431a3e142338da0511b9`, then `55f75cbaf7db7b2715f639c14d09a508d9a78932`. Local ancestry joins all four to55; its gitlink is e1. This is local object/ancestry proof plus the coordinator's landing report, not a fresh remote publication query or a full-validate result. Local provider working heads were H25781c5c48d47c316324fd912b7dcafa394c339a and R42adcb1d44f38477e14a073b135ceccecae0a7a5, older ancestors. A later cached H origin/main abfc6d98aa60a1dedbfeb717092fd21ec6ec61ed was observed, not selected.

Fresh owned linked worktrees are `hermit-stats-capture` and `reverie-stats-capture` in this directory. They share only the existing slot-owned Git stores. The preserved D3 H74689bb06d184c19837022eb9cad03dff919981f and R0d054e2abd05e0572bf080c88307072a44ebb0c7 source trees remain clean and unchanged.

## Exact ownership change (revised compatibility boundary)

The initial untested inside-container/raw-IPC draft is preserved in `draft-in-container-superseded/`. Review found that it changed custom-TMPDIR placement. The coordinator directed this smaller change, preserving the old visible path, prefix, bind and completion boundaries.

- R `reverie-dbt/src/launcher.rs` promotes the existing resource to `DbtStatsSink`: new() retains the existing R prefix/TMPDIR; with_prefix() accepts H's existing `hermit-dbt-verify-stats-` naming choice. The type privately owns TempDir/path, configures the native argument, exposes a borrowed directory for the existing bind declaration and captures the completed stream. Existing physical run/input/cleanup/convenience APIs stay intact.
- R `backend_stats.rs` provides presence-aware `DbtStatsCapture::{Missing, Unreadable{message,raw_os_error}, Records(Vec<u8>)}` and shared read/decode. Legacy `_and_stats` still maps missing to an empty snapshot explicitly; strict callers retain presence. No serde/protocol addition is needed.
- H `backends.rs` deletes its duplicate TempDir/path/configure/read/aggregate implementation. The original outer factory returns the shared sink using the old prefix and error context; the original DbtExecution statistics/bind declaration retains it. Capture still finishes after normal physical cleanup and at the old evidence/latch/first-status boundary. H keeps strict missing/empty/malformed/inconsistent counters, terminal replay and complete evidence policy.
- H `verify_run.rs` reads saved DBT_STATS via the shared reader and same strict H policy. The saved writer borrows the exact captured bytes; no aggregate re-encoding or second read. Existing constructor/guest/error precedence and lifetime remain.

No raw records cross new IPC, no new filesystem-visible location, no guest environment rewrite, no new size limit or comparator relaxation. Native records/wire/counters and the execution driver are unchanged. The thin borrowed-directory mount declaration is compatibility glue, not a second file owner.

## Why not the other slices first

R `reverie-inguest/src/tool_host.rs::drive_tool_syscall` has synchronous Return/Exit/ForkChild outcomes and a spinning Pending poll; current DBT `run_tool_syscall_with_memory_reader` needs `ExecuteOriginal` after releasing the native callback borrow and completing the physical original syscall. Installed Tool publication does not supply that deferred contract. D2 therefore remains a shared T execution-API dependency, not a caller rename. The latest read T status has native exec components under source integration and no qualified connected successor chain; do not target an unlanded private API.

Physical DispatchStats is separate: current DBT `runtime_totals.syscalls` counts Rust Tool callbacks, not all native DR interceptions. The shared schema only has five signal/patched/ptrace delivery routes, while DBT returns None. No unavailable field becomes zero and no translated route is mislabeled as a patch/trap. A later meaningful projection needs an actual native measured producer and shared optional representation.

Shared DETLOG/protected transport/max-log-bytes is also later: DBT has no actual attachment to the shared protected-descriptor owner. Moving only INFO cannot replace complete image/FINAL/loss evidence. No descriptor protocol, new runtime model or DR wire/ABI change enters this capture refactor.

## Validation scope

See `stats-capture-check-plan.md`, `default-feature-check-plan.md` and independent `architecture-review.md`. The six delegated R tests exercise real sink presence/read errors/raw order/cleanup and actual Reader/Inherit/status completion with output/Global retention. H retains all strict and run-precedence assertions and extends the existing whole-tree capture test through the actual saved-run writer/shared offline reader with unequal binary/multiline records. The original strict32-byte EOF, held-open input, terminal and saved/offline cases stay in scope. Default H lib/bin tests plus workspace/all-targets/all-features Clippy prevent the previous cfg(test) hole. Full transport rebuild is conditional on transport changes; none is proposed.

This is a behavior-preserving refactor. A missing new API on the base is not a failing-runtime regression. Existing behavioral controls and new ownership/fault-injection regressions establish equivalence; literal failures remain evidence if found. New test populations/inventory are measured rather than projected. No builds or runtime outcomes are claimed by this source plan.
