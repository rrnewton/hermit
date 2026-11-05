# D3 procfs library stack overflow

Requested fix base: Hermit `00591c8a43e5d25bc50547ad568184eceb41f417`, with Reverie `16ffab7c558c47c78c43aa8e0e4242eb2deeeb3d`. The isolated source is `hermit-main00591`; the existing slot-owned object store already contains the base, so no fetch or provider mutation was needed. Statistics H342daf/Rff30 remain clean and parked.

The coordinator reports both `procfs_determinism` tests failing 3/3 on devbig014 with `third-party-backends,kvm-execution-tests`: `chroot_mountinfo_subset_keeps_fdinfo_identity_consistent` and its `_with_readonly_proc` wrapper. Reported bisect: good `2a6b3c87e25d8917fee58686fd56c57cfee81639`, bad `9f57cc36bf13e0484be50e141384be6f3bd9996d`. The original command/profile/log evidence has been requested; these statements remain coordinator evidence until joined to actual artifacts. The sole executor owns exact reproduction and any compiled frame evidence.

## Initial source comparison

The bisected D3 change adds immutable external-read provenance, root/image-bound initial input and a readv external-read route. `DetFd::inherited_external_read` still calls the existing direct `with_description` access; there is no newly recursive accessor or metadata discovery chain in the bounded diff. The ptrace backend does not take the shared descriptor-table initial-input install path.

The failing fixture enters a private namespace and invokes `hermit::run_with_output` from its libtest thread. It reaches `run_with_output_backend_timeout`, then `ptrace_completion::run`. That owner calls `make(control.clone())` to construct its future by value, then boxes it. The future contains `dispatch_output_backend`, including all compiled backend branches. The bisected change enlarges `ExecFdState` and `GlobalState` carried in the async graph; source growth can therefore affect the ptrace caller even when no controller-input seed is offered.

This is a **frame-growth hypothesis**, not yet an observed cause. The fixture, library wrapper and completion owner are byte-identical across the reported good/bad pair. Existing non-ptrace status dispatch already boxes the large backend future before timeout/Tokio wrappers; output dispatch has no corresponding boundary. Compiled stack/prologue evidence must distinguish construction overflow from recursive polling before selecting a fix.

## Actual unchanged before-state

The executor reproduced both failures on devbig014 at exact H00591/R16ff using ordinary default-profile Cargo:

```text
cargo test --offline --locked -p hermit --features third-party-backends,kvm-execution-tests --test procfs_determinism chroot_mountinfo_subset_keeps_fdinfo_identity_consistent -- --nocapture
```

At 2026-10-14T12:17:48Z it returned 101: 2 selected, 0 passed, 2 failed, 49 filtered and none ignored. Each private-namespace child reports the original test thread overflowing its stack; the parent observes `Signaled(SIGSEGV, true)` instead of `Exited(0)`. No product change, stack/thread override or extra diagnostic environment was used. The coordinator's original invocation is still unjoined, so this is an actual default-Cargo reproduction rather than a claim of identical settings to that earlier run.

The exact command, stdout/stderr and before-state summary are under `validation-plain-main00591/repro-default*`. The retained test ELF is `validation-plain-main00591/artifacts/procfs_determinism-3a0f05b3fbc3b749`, SHA256 `b75f91a8d8291bd97dafd1ce75d7be07be0e70f66411b185611ca7b76b52230e`. Reviewer analysis of that compiled artifact is pending. The proposed common output future boxing is only an unapplied draft at `output-future-boundary-draft.patch`.

No product edit or test relaxation has been made. Stack limits, test assertions, features, backend selection and native semantics must remain unchanged. The requested acceptance is the two original fixtures each passing 3/3 at the fixed source on devbig014; it is not a full validation or canonical-parity claim.

## Retained-ELF frame evidence and controlled experiment

The reviewer measured the exact before ELF, rather than inferring frame size from source: the ptrace completion entry allocates 73,352 bytes, the output/skid wrapper poll 69,512 bytes, the dispatch-output poll 254,472 bytes, and the Detcore syscall-event poll 565,672 bytes. The completion constructor and ordinary task poll measured 99,624 and 95,400 bytes. These are individual compiled prologues, not a claim that every frame is simultaneously live or that any one frame is 2 MiB. The retained callsite join connects completion construction/drive to output wrapper construction/poll, then ptrace wait_with_output construction/poll. No new recursive source edge was found.

The reviewed draft boxes the three common output-dispatch boundaries. It keeps ptrace future construction after admission, polling/drop on the original thread, skid result order, and timeout semantics. The root authorized this as a controlled code experiment following one bounded retained-ELF debugger attempt. Its sufficiency remains pending the identical test rerun and compiled after-frame comparison; no source commit is authorized before that evidence. The original before ELF and failure logs remain immutable.

## Applied experiment boundary

The one retained-ELF GDB attempt ended before test execution with the installed GDB 9 internal `stap-probe.c:1293` assertion. It produced no usable dynamic backtrace; its command/output and limitation remain under `validation-plain-main00591/retained-elf-backtrace.*`. No debugger setup retry or guest-observability change was made.

After executor source release, the reviewed draft was applied only to `hermit-cli/src/lib.rs` on the unchanged base. The Git diff SHA256 is `dbd8b0efa1c7188715901a3b1c8bf2298e5f04aa203e56a56fc44db85be182e9`; the after-image SHA256 is `6fcdf988e3e8ddbb18ea3abb7786d32bdab03d037be3d2cae25739f1fe9a6b1e`. Exact source attribution is retained in `output-future-boundary-source.json`; no commit exists yet. The executor owns the identical first two-test rerun and after ELF. If that is green and the reviewer supports the frame/source join, the authorized sequence is one clean commit on00591, then the requested three independent test rounds and finish checks at that committed head.

## First patched result and committed source

The unchanged two-test Cargo command passed both selected tests at the reviewed dirty-source experiment (exit 0, 49 filtered, none ignored). This result is separate from final-head acceptance. The retained after ELF has SHA256 `cd7c0411ff161efd0be3b6292ccfe4a76523a1351031af4f1b0a7beec67d3008`.

`frame-comparison.json` joins the two ELF hashes. The ptrace completion frame fell from 73,352 to 6,232 bytes and the output/skid poll frame from 69,512 to 36,008 bytes. The non-ptrace output constructor fell from 105,272 to 4,584 bytes and its poll frame from 171,000 to 35,592 bytes. The original test frame stayed 8,456 bytes; the deep dispatch and syscall-event poll frames stayed 254,472 and 565,672 bytes. These individual static frame measurements, the unchanged semantics and the identical-command fail/pass experiment support the heap-boundary repair. They do not supply a dynamic overflow backtrace or a simultaneous stack-total measurement.

The source is now one clean commit, `6611e75333eba5682a21aa9b5e54444eb32747dc`, directly on `00591c8a43e5d25bc50547ad568184eceb41f417`, with the same R16ff pin. It changes only `hermit-cli/src/lib.rs` (22 added, 30 removed lines) and is byte-equal to the successful experiment patch. Precommit paths/numstat and fresh literal disclosure are retained under this ignored investigation directory. Author and committer are hermit-bots. No tests, features, stack settings, timeout limits or provenance implementation changed.

The sole executor now owns the requested three independent original-test rounds at the committed head, followed by default/DBT Hermit unit suites, the existing 11 CLI cases, terminal coverage and the authorized lint/docs/format checks. Those final-head results are pending; no full validation or cohort/flake-rate claim is implied.

## Original runner clarification and final-head repetitions

The coordinator has now confirmed that its original bisection used DEBUG/unoptimized, debuginfo-enabled **Nextest**, with the same third-party/KVM execution features and the procfs target. Our actual before/first experiment used ordinary default-profile **Cargo test**, also unoptimized, and remains labeled as such. No historical receipt is relabeled as the coordinator's original runner.

At committed H6611 the executor completed three independent repetitions of the original Cargo test command, with both selected tests passing in each round. Because the runner clarification arrived afterward, it will additionally run three fresh DEBUG Nextest rounds of the two tests at that same committed head, after the active default-H unit compile. Those matching-runner acceptance results remain pending.

## Matching-runner acceptance

At exact committed H6611, the executor completed three fresh DEBUG Nextest rounds, each 2/2 passed, 49 outside the selection, exit 0. The build profile was actually unoptimized with debuginfo; stack settings and source remained unchanged. The default Hermit lib/bin suite also passed 896/896 with no skipped tests. DBT/CLI/terminal/lint/docs/format completion remains pending.

## Completed exact-head checks

All scheduled checks are closed at clean H6611/R16ff: three DEBUG Nextest procfs rounds (2/2 each), default units 896/896, DBT units 936/936, CLI 11/11, terminal 6/6, required all-feature/all-target workspace Clippy, docs with warnings denied and formatting. The terminal DBT cases use `--log=debug`; no log-level assumption is inferred from the other selected tests. The final handback joins the sealed executor and adversarial review notes. There was no count, pin, test or generated-file change. No further source work is authorized or necessary for this isolated repair.
