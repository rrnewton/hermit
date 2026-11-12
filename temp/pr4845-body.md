[hermit2, vmakaev-metamate, claude-opus-5-5, devvm53386, role=impl]
Owner: musedrain (muse, devbig014) since 2026-11-07T09:00:00Z

## Plain Language Summary and Project Impact

`--chaos-instruction-offset` (https://github.com/rrnewton/hermit/pull/4769, landed on main as `f9a1fcd3b184e6b63736a946d1017933f30e689c`) lets chaos preempt inside straight-line code: a chaos timeslice ends `k` single steps past its branch target. Only per-thread preemption records carried those decisions; schedule traces dropped them, so `--replay-schedule-from`, `hermit analyze` and `hermit bisect` refused any run that used the option. This change records each offset decision in the schedule trace and replays it: the timer's batch of branches carries the same `(rcb_target, k)` the row holds, and a schedule replay arms that timer again. analyze and bisect accept such recordings and keep the offsets in the schedules they build.

Records without offsets are written as main writes them. Records whose schedule trace holds offsets state the trace-offsets format (7), above the offset-rows format (6, `INSTRUCTION_OFFSET_ROWS_FORMAT`); the newest readable record format (`PREEMPTION_RECORD_FORMAT_MAX`) becomes 7 while rows alone stay fenced at 6 and the offset-free format stays 5 (`PREEMPTION_RECORD_FORMAT_WITHOUT_OFFSETS`).

Rebase (2026-11-07, musedrain): the 14 own commits `65f7a7a7d6bca0d728d111d566562cda9c1be4b9..e1ddf3b67689816c43a09d925538d6cad80def8c` are replayed onto current main `ec4e8b597e7bec0a254090fd7727a84c3e9d3666` as branch `drain/4845` at head `e7e4d8cfb5f49c1b7b59e17ae5b3d153e92b4833` (21 commits: the 14 plus seven rebase adaptations and delta-review restorations). The bottom 23 commits of the original PR duplicated landed 4728/4769 and were dropped; the PR's Reverie pin commits were dropped (main's pin carried). Conflict resolutions: scheduler/tool_global/record paths composed both sides (main's landed RPC rows 47-51 kept, this change renumbers to 52 with guard-measured digest); schedule-trace offsets are format 7, rows alone format 6; analyze/bisect call sites take the PR's refuse-unread-then-adopt sequence over main's refuse-any-offset policy; the seekable record reader probes an undecodable record's format as `decode` already did. No queue submission accompanies the rebase; a fresh dual delta review at this head comes first.

Full pre-condense body (145874 bytes, sha256 `beae56c4545b0cda238f200fd9796b3bd21cadf812d05597d3597a5fa1ad1d85`), including How it works, Record format and versions, Refusals, End-to-end, Not done, and the full Validation history, is archived byte-faithfully in dev-hermit at `ai_docs/archived/evidence/2026-11-06-hermit-4845-evidence/` (commit `4fb59053ac4cd6dd46b192116bb0bd0429c59d70` on dev-hermit main).

## Determinism

Recording: the offset is the one 4769 already armed and recorded in the row, a pure function of (scheduling seed, thread, draw index); attaching it to the batch reads only that decision and the branch clock (retired branches). Replay arms the same timer at the same target with the same step count, so the thread stops at the same instruction; a replay that cannot reach the recorded point desyncs or records the retryable skid marker, never silently lands elsewhere. The offset travels with its timeslice through the same Continue / context-switch / grant paths and is dropped wherever the timeslice is dropped; no host timing enters. analyze trials draw no offsets and replay without chaos when the analysis carries offsets. Wire compatibility: `SchedEvent::instruction_offset` and `GlobalResponse::WithReplayInstructionOffset` change the global RPC, so the change takes the next unreserved revision above main's landed 52 (53; reserved 33-39 and 42-44 unchanged), with the digest measured by the RPC guard at the rebased head (`(53, "13e87b8c8d8f9a42", "f013e36743d8d9e6")`, guard test passing at the head below). Virtual time stays continuous: offsets refine where a slice ends inside straight-line code; nothing is frozen, rounded or coarsened.

## Linux Semantics

Nothing the guest observes through a syscall, a signal or /proc changes. The change records and replays where 4769's preemptions land, using the ptrace backend's existing precise-timer single steps. No kernel behaviour is emulated differently, so there is no kernel source to cite (as the original body stated; none fabricated).

## Validation

## Test plan

Exact head: `01c29f1f5408f2552ef74eed4fc46a71acf01ade` on `drain/4845`, base hermit main `d052efbbbf81` (4759 landed; this change renumbers to RPC 53, digest guard-measured `(53, "13e87b8c8d8f9a42", "f013e36743d8d9e6")`, composed with 4759's `TraceSchedEventResponse::backstop`). All runs at that head (ptrace backend, default log level, no relaxations; `--offline --locked`):

- `cargo test -p hermit-detcore --lib`: 3935 passed, 0 failed (includes the RPC revision guard: row `(52, "9c1309300feecc01", "f013e36743d8d9e6")` matches its definitions).
- `cargo test -p hermit-run --test cli -- an_instruction_offset_record_replays_from_its_schedule_and_analyze_verifies_its_endpoints`: 1 passed. The case now uses fixed seed 1 at 30,000 guest iterations (same assertions): with the harness-compiled guest it loses 17,883 of 60,000 increments, while seeds 1-8 lose none at 20,000. Red/green in the validation profile: on the pre-restore tree fb5b7b6b the schedule replay desynchronizes at event #305 and the case fails; at this head it passes (33.03, 28.06, 28.39 CPU-s in its cgroup across three runs). Its exact PMU cap is 52 CPU-s (above 1.5x the largest fixed-seed sample), and the node's budget is recomputed at 285 from the measured 238.28 CPU-s 30-case total (both cited in the node description).
- Why the retained workload exposes the race: measured per seed at 30,000 iterations (a third of the case's 100,000), 6 of 8 seeds lose updates (seeds 2/8 lost 30,000 of 60,000; seed 4 lost 24,982; seed 7 lost 22,067; seed 5 lost 19,260; seed 6 lost 10,568; seeds 1/3 lost 0), so the case's chosen losing seed is genuinely racy at any size at or above that.
- `cargo test -p hermit-verify` (bin): 52 passed, 0 failed.
- `cargo run -p hermit-manifest-plan --bin generate-validation-dag -- --check`: canonical. DAG: 1 intended entry carried (the offset schedule-replay case routed to `privileged-test.pmu_cli_cases`, 29 -> 30, skipped in `test.cli`/`test.cli_on_host`), verified.
- `cargo fmt --all -- --check`: clean. `cargo clippy --workspace --all-targets -- -D warnings`: clean.

Pre-rebase validation at `e1ddf3b67689816c43a09d925538d6cad80def8c` and the revision history are in the archive above (not repeated here). musereviewc's Muse content review at `e1ddf3b6` found no blocking issue; it is not a landing stamp. Its delta review at `fb5b7b6b` requested changes (six findings, all rebase drops); all six are restored from `e1ddf3b6` at this head, including every named test. Dual delta review at `e7e4d8cf` is owed before queue submission.

Adapted assertion, stated plainly: in restored `analyze_carries_instruction_offsets_through_its_trials`, the old assertion expected a schedule replay from an offsets-free analysis to keep chaos (`chaos == true`); the restored one expects `chaos == false` for both cases, because main's `f98d92f70a4` made `with_schedule_replay_from` chaos-free for every schedule-replay trial. That landed change, not this PR, decides the flag; the offset assertions are unchanged. No full validate is claimed here; the merge queue's full-profile validate at the exact head is the landing gate.

## Human Review Required

Trigger 4: a core DetCore scheduling change affecting how programs are scheduled (schedule replay arms recorded instruction offsets; analyze and bisect replay them for race search). No new syscall support is added, so no syscall audit tags apply. Dual delta review at the rebased head is required before submission.

## Relationship to gVisor

None. This change touches no KVM behaviour; the KVM backend is unchanged. (The original stack carried a Reverie pin commit touching `hermit-run-kvm/Cargo.toml`; the rebase onto current main carries main's pin.)
