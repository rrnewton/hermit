# Procfs stack regression: bounded source review

Reported good H `2a6b3c87e25d8917fee58686fd56c57cfee81639` / R `6dd1d62bf71ae171945feed82154a43aedf555bf`; reported bad H `9f57cc36bf13e0484be50e141384be6f3bd9996d` / R `e1ecaeb31a0bb1f8b86f36e769296ab6a7de3782`. The assigned current repair base is H `00591c8a43e5d25bc50547ad568184eceb41f417` / R `16ffab7c558c47c78c43aa8e0e4242eb2deeeb3d`. Review is read-only source/call-graph and retained-ELF analysis; no build, reproduction, stack-size change or product edit by this reviewer. The executor's fresh two-test beforestate is joined below; the earlier three-failure report remains supplied evidence, not an independently executed result here.

## Current finding and limits

No new recursive call edge was found in the changed descriptor access path. `DetFd::inherited_external_read` calls the existing non-async `with_description`, which locks the existing local/shared OFD model; the local arm invokes its closure directly. `ThreadState::with_detfd` discovers current-process descriptors only for tools sharing the guest descriptor table. Ptrace instead initializes metadata through `setup_stdio`, with that capability false. The newly installed input lease is prepared only by the DBT Reader launcher; ptrace's `GlobalState` starts with `inherited_input=None` and does not call its installation hook. Initial-image adoption remains capability/root/image gated. Unknown FIFO or fd-zero promotion therefore is not a supported explanation of this ptrace regression. This rules out those new direct edges, not every possible unrelated recursion in the program.

There is a concrete static-frame amplification hypothesis. The new shared state increases inline `ExecFdState` (initial input identity), `GlobalState` (pending lease), and `OpenFileDescription` (type/access provenance). Requests/responses and awaited handlers carry portions of this state. `handle_readv` also gains an inline awaited blocking-I/O branch. The retained ELF now establishes the individual function prologues listed below; it does not establish a simultaneous stack sum or exact good-to-bad size delta.

The unchanged inner libtest fixture calls `hermit::run_with_output` on the original test thread after entering its private namespace. That delegates to `run_with_output_backend_timeout`, then `ptrace_completion::run(make)`. At bad-H `hermit-cli/src/ptrace_completion.rs:518-519`, `make` returns the concrete future by value before it is boxed. That future awaits `dispatch_output_backend`, whose async state contains all compiled backend alternatives, even though this invocation selects ptrace. A frame-size increase can thus cross the original libtest stack bound without dynamic recursion or entering the DBT branch. The status-returning non-ptrace wrapper already heap-owns `dispatch_backend` before its deadline/Tokio wrappers (`hermit-cli/src/lib.rs:3945-3956`); the output wrappers do not.

The procfs fixture, `hermit-cli/src/lib.rs`, `hermit-cli/src/ptrace_completion.rs`, and Cargo profiles are byte-identical across the reported good/bad boundary. At 00591 the two selected chroot functions remain unchanged; other procfs mount-target fixtures changed independently. The validate profile remains release optimization with debug assertions and overflow checks enabled. R's exact good/bad delta is six inherited-input/core/DBT paths, with no ptrace source change; its new default installation hook is infallible and unused by ptrace.

## Smallest candidate, conditional on frame evidence

If the executor's exact ELF/backtrace confirms oversized async construction/poll frames, heap-own the common output-dispatch future before it is captured in the ptrace/skid/deadline wrapper, using the existing status-wrapper ownership pattern. Preserve construction and first poll inside the admitted ptrace operation, inline original-thread polling/drop, retained-operation cleanup, result/status/summary order, and deadlines. Do not add a thread, spawn another driver, enlarge test stacks, relax descriptor provenance, disable a backend feature, alter scheduler turns, or bypass the original test. Final placement should follow measured frame evidence; this note is not a claim that such a patch has fixed the failure.

The exact failed binary/profile and disassembled prologues are now retained below. A dynamic backtrace and fixed comparison remain the executor's responsibility. The original coordinator invocation/profile has not been established; fresh default-Cargo reproduction is explicitly separate.

## GOALPOST-MOVING REVIEW RULE

Adversarial reviewers must look explicitly for goalpost moving. We are extremely skeptical of any goalpost moving. YOU DO NOT CLEAR THE BAR BY SIMPLY LOWERING THE BAR.

Treat each of these as an explicit review target:
- weakening an assertion so a test passes
- widening a tolerance · adding an exemption · skipping a case · relaxing a comparator
- renaming or relabelling so a failure reads as a pass
- deleting a check rather than satisfying it

The exact three-wrapper draft is source-clear for the authorized controlled experiment, not cleared as an established runtime repair. An acceptable storage-only repair leaves both exact test assertions, original thread stack, features/profile, comparator, flags and timing semantics intact. Reported stack-overflow/SIGSEGV attempts remain failures. A heap-placement repair by itself adds no syscall support, determinization policy, Tool/Guest/Backend API or scheduling policy; reassess numbered triggers against the actual patch rather than describing it merely as backend work.


## Fresh beforestate and finite compiled-frame join

The closed `validation-plain-main00591/repro-default-command.json`, `repro-default-summary.json`, and `repro-default.result` bind clean H `00591c8a43e5d25bc50547ad568184eceb41f417` / R `16ffab7c558c47c78c43aa8e0e4242eb2deeeb3d` on devbig014. The command is `cargo test --offline --locked -p hermit --features third-party-backends,kvm-execution-tests --test procfs_determinism chroot_mountinfo_subset_keeps_fdinfo_identity_consistent -- --nocapture`, with the recorded nice/affinity/build environment, no stack or test-thread override. Both selected tests failed: 0 passed, 2 failed, 49 filtered, 0 ignored, exit 101. The named namespace test thread overflows and the parent observes `Signaled(SIGSEGV, true)`. This is the actual default Cargo test profile, not an asserted match to the unavailable original coordinator invocation.

Read-only analysis uses retained ELF `validation-plain-main00591/artifacts/procfs_determinism-3a0f05b3fbc3b749`, SHA-256 `b75f91a8d8291bd97dafd1ce75d7be07be0e70f66411b185611ca7b76b52230e`, 516,941,136 bytes. `before-frame-disassembly.json`, `before-syscall-prologue.txt`, `before-large-prologues.json`, and `before-wrapper-callsite-relocations.json` retain the finite selection. Prologue sizes count explicit x86 stack subtraction/probe loops, excluding pushes/return addresses; they are individual static allocations, **not a simultaneous stack total**. The large-prologue index is a candidate census; the following decisive frames were directly disassembled.

| Function, before ELF | Address | Explicit stack allocation |
| --- | --- | ---: |
| Original procfs test function | `0xe67560` | 8,456 B |
| `dispatch_output_backend` future constructor | `0x173cb50` | 88 B |
| `ptrace_completion::run<Output,F>` | `0x183a160` | 73,352 B |
| Output wrapper future poll | `0x1c6dcb0` | 69,512 B |
| `dispatch_output_backend` future poll | `0x1c715b0` | 254,472 B |
| Non-ptrace output-inner wrapper constructor | `0x1741440` | 105,272 B |
| Non-ptrace output-inner future poll | `0x1c76690` | 171,000 B |
| Detcore syscall-event future poll for ptrace `TracedTask` | `0x1ac0b30` | 565,672 B |

The syscall-event prologue is `0x8a000 + 0x1a8`; the dispatcher is `0x3e000 + 0x208`. No single 2 MiB frame is claimed. The constructor itself writes the future through its return pointer; its 88-byte frame must not be confused with the much larger poll frame/state.

The exact callsite join agrees with source: retained operation `run` calls make at `0x183a9f3` and later drive at `0x183af35`; the wrapper constructs dispatch at `0x1c6dec8` and polls it at `0x1c6df88`; the dispatcher's ptrace arm calls `wait_with_output` at `0x1c74c26` and polls it at `0x1c74d28`. Indirect addresses were resolved from this ELF's relative GOT relocations and symbols. This is static callsite evidence, not a captured runtime backtrace. No repeated recursive frame sequence has been observed, and the inspected changed source introduces no direct recursive edge. Static frame amplification is supported; exact dynamic exhaustion and good-to-bad size growth are not yet proven.

## Exact candidate disposition

`output-future-boundary-draft.patch`, SHA-256 `127ebce788b6bc7de666c35148dc19ef7e7a3f7c9ae0ee145486c1a659bc010e`, changes only three output future ownership boundaries in `hermit-cli/src/lib.rs`. Both ptrace variants still create/poll the backend after existing admission, under the same current-thread runtime and retained-operation owner. `SkidOvershootReport::begin`, `finish`/`finish_with_count`, config preparation, control ownership and error propagation retain their order. The non-ptrace branch still refuses unsupported timeouts first, then polls the same guest on the same thread; timeout expiration drops that guest future at the same enclosing completion boundary and returns the same `GuestTimedOut` error. No task/thread spawn or scheduler policy is added. Heap allocation adds an allocation-failure possibility, as existing boxed status/retained-operation paths already have; it does not pretend to bound every backend's stack.

No assertion, comparator, tolerance, exemption, skip, test label, backend selection, stack size, timing bound, or source provenance guard changes. The original red attempts remain red. Numbered post-facto triggers: this storage-only patch adds no (1) syscall, (2) Tool/Guest/Backend abstraction, (3) determinization strategy, or (4) scheduling-policy change. The prior shared-input change's triggers remain independent.

The draft is source-clear for the coordinator-authorized controlled experiment. Its sufficiency is deliberately pending unchanged-test outcomes and direct rebuilt-frame comparison. The existing dominant syscall-event frame remains relevant; reducing outer wrappers is not itself proof that every overflow has been eliminated.


The executor's single bounded offline-ELF debugger launch ended before test execution: GDB 9 aborted internally with `stap-probe.c:1293: sect_index_data not initialized`, exit 134. `validation-plain-main00591/retained-elf-backtrace.{command.json,gdb,stdout,stderr,result}` retains that attempt. It supplies no usable runtime backtrace and is not a second test result or evidence against recursion. The before ELF remains unchanged; no debugger retry is required for the authorized controlled experiment.


## Controlled afterstate and frame comparison

The executor's closed `boxed-default-r1-command.json` and `boxed-default-r1-summary.json` bind the single uncommitted `hermit-cli/src/lib.rs` after-image `6fcdf988e3e8ddbb18ea3abb7786d32bdab03d037be3d2cae25739f1fe9a6b1e` on the same H00591/R16ff. The recorded argv, prefix, cwd, explicit environment and unset environment equal the fresh failed beforestate. Both original tests now passed: 2/2, 49 filtered, 0 ignored, exit 0. This is the first controlled experiment, not yet the later exact-commit acceptance rounds.

After ELF `validation-plain-main00591/artifacts/procfs_determinism-boxed-3a0f05b3fbc3b749` is SHA-256 `cd7c0411ff161efd0be3b6292ccfe4a76523a1351031af4f1b0a7beec67d3008`, 516,950,944 bytes. The reviewer computed that hash and directly compared the corresponding disassembled prologues, retained in `after-frame-disassembly.json` and `frame-comparison.json`.

| Corresponding function | Before stack allocation | After stack allocation | Change |
| --- | ---: | ---: | ---: |
| Original procfs test | 8,456 B | 8,456 B | 0 |
| Output dispatcher constructor | 88 B | 88 B | 0 |
| Ptrace retained-operation `run<Output,F>` | 73,352 B | 6,232 B | −67,120 B |
| Ptrace output wrapper poll | 69,512 B | 36,008 B | −33,504 B |
| Output dispatcher poll | 254,472 B | 254,472 B | 0 |
| Non-ptrace output-inner constructor | 105,272 B | 4,584 B | −100,688 B |
| Non-ptrace output-inner poll | 171,000 B | 35,592 B | −135,408 B |
| Detcore syscall-event poll for ptrace | 565,672 B | 565,672 B | 0 |

The reduced ptrace retained-operation prologue is `0x1000 + 0x858`; its wrapper poll is `0x8000 + 0xca8`. The unmodified dispatcher/syscall frames do not need to shrink for the outer storage repair to reduce live nested stack pressure. No combined peak is inferred from this table. There is no new or removed recursive call edge in the patch.

Disposition: the isolated storage-only change, unchanged original tests switching from failure to success, and corresponding compiled-frame reductions support the proposed shared output-future boundary as a principled fix for the reproduced default-Cargo failure. It is source-clear for the authorized clean commit and original-stack repeat validation. This conclusion does not substitute for the requested committed-head rounds or assert a backtrace we do not have. Assertions, flags, timeouts, comparator, backend selection and test-stack configuration remain exact.


## Committed source join

Independently read clean final source H `6611e75333eba5682a21aa9b5e54444eb32747dc`, parent `00591c8a43e5d25bc50547ad568184eceb41f417`, unchanged R `16ffab7c558c47c78c43aa8e0e4242eb2deeeb3d`. `review-committed-source-join.json` records that the only path is `hermit-cli/src/lib.rs`, its commit diff is byte-equal to the tested experiment (`dbd8b0efa1c7188715901a3b1c8bf2298e5f04aa203e56a56fc44db85be182e9`) and its after-image remains `6fcdf988e3e8ddbb18ea3abb7786d32bdab03d037be3d2cae25739f1fe9a6b1e`. No new source review is required for this metadata-only transition from the reviewed working diff. Committed-head acceptance rounds and finish checks are still pending at this note update.


The coordinator subsequently identified the original runner: Nextest selecting `-p hermit --features third-party-backends,kvm-execution-tests --test procfs_determinism`, default unoptimized plus debuginfo, with no validate Cargo profile. The supplied original report path is `/home/newton/work/dev-hermit/worktrees/slots/kvm-20261012/ignored/runs-perf/KD/procfs-determinism-main.out`; it was absent at that local path, so original runner/profile attribution is coordinator-confirmed rather than independently read from that log. Our failed beforestate and first green experiment use the same default build profile but Cargo's test runner; they are not relabelled as Nextest attempts. The requested three committed-head acceptance rounds use DEBUG Nextest. Other independently requested suite/lint checks may use their maintained validate profile and do not substitute for this stack regression's debug-profile acceptance.


Attribution boundary: the coordinator's supplied first-good/first-bad bisection is H2a6b3c87/H9f57cc36. Direct Git reads confirm those heads pin R6dd1d62b/Re1ecaeb3 respectively; the bad H commit changes Reverie pins, lock/manifests and integration metadata as well as Detcore fields/readv/provenance. This review measured only current H00591 before and its storage-only H6611 successor. It did **not** compile or compare the historical good/bad ELFs, and assigns no numeric growth to any one D3 field, syscall branch or accompanying Reverie change. The supported mechanism is excessive common-wrapper storage relieved by heap ownership in the reproduced default-debug path. The field/layout observations are source-level origin hypotheses, not isolated causal measurements. No extra bisect build is needed to state this bounded result honestly.


## Committed debug Nextest acceptance

Independently joined `commands-head-nextest-debug.json`, all three closed `head-nextest-debug-r{1,2,3}.result` and `.stderr` files, and the sealed summary. Every command names exact H `6611e75333eba5682a21aa9b5e54444eb32747dc` / R `16ffab7c558c47c78c43aa8e0e4242eb2deeeb3d`, features `third-party-backends,kvm-execution-tests`, the original `procfs_determinism` target, and exactly the two reported test names. No `--cargo-profile`, stack size, test-thread override, added log level, or optimization switch is present. Every compiler line explicitly says `Finished test profile [unoptimized + debuginfo]`.

Each independent round ran both tests and passed 2/2 with exit 0; elapsed Nextest summaries are 0.115 s, 0.129 s and 0.111 s. The other 49 target tests are outside this exact two-test selection (Nextest labels them skipped); they are not credited as passed, and no test was newly annotated ignored. This meets the requested three-round reproduction acceptance at the clean committed source. It is ptrace procfs regression evidence, not a new L2/L3/L4 verification cohort. The original default-Cargo 0/2 failure, first green uncommitted Cargo experiment and these committed DEBUG Nextest rounds remain separate facts.

The narrow repair is now supported by original-stack runtime acceptance and independently verified wrapper frame reduction. Additional default/DBT unit, lint, format and other finish results remain separately scoped to their actual commands/report when sealed.


## Final completed evidence review

At the final read, H `6611e75333eba5682a21aa9b5e54444eb32747dc` remains clean on `00591c8a43e5d25bc50547ad568184eceb41f417`, with unchanged checked-out and pinned R `16ffab7c558c47c78c43aa8e0e4242eb2deeeb3d`. The reviewer verified every size/hash in the sealed 70-file evidence index (`a5adcb1bd6f3beac8aa9ee5459a7e672fbcd39e6ff503a7679e6e606bab3c2d4`), read all final command records and literal results/summary lines, and rehashed all five production-linkage artifacts. `review-final-evidence-join.json` retains this finite join.

The already-reviewed three DEBUG Nextest rounds each passed both original procfs tests at that head. The executor also retained three committed-head runs of the original default-Cargo reproduction command, each 2/2. These are separate attempts, not a relabelling of the failed beforestate.

| Additional completed check | Exact result/scope |
| --- | --- |
| Hermit default-feature lib/bins | 896/896, 0 skipped; default cfg really compiled/executed |
| Hermit DBT-feature lib/bins | 936/936, 0 skipped |
| Eleven exact live/saved/offline CLI cases | 11/11; 407 cases outside selection remain uncredited |
| Inherited terminal target | 6/6: three real DBT cases plus three native controls |
| Workspace/all-targets/all-features Clippy | exit 0 with `-D warnings` |
| Hermit all-features docs | exit 0 with `RUSTDOCFLAGS=-D warnings`, `--no-deps` |
| Workspace rustfmt check | exit 0 |

The broader unit/CLI/terminal commands use the maintained validate Cargo profile. They do not replace the unoptimized DEBUG procfs acceptance. `HERMIT_REQUIRE_DBT=1` is present for CLI/terminal. Production evidence binds the current CLI, exact-R16ff Release native client, current rebuilt `libdetcore_dbt.so`, CMake source/runtime paths and NEEDED/RPATH. All five current artifact hashes equal the captured records. Reuse of the unchanged DynamoRIO SDK cache is not a fresh SDK build claim.

Assurance is fixture-specific. The original `/bin/cat` EOF case uses DBT, default logging, `--strict --verify --verify-strict`; it asserts exact 32-byte output/hash and exit 0 on both runs, canonical/bitwise parity, I/O buffer comparison and nonzero INFO counts on each side. Its temporary JSON is deleted by the unchanged fixture; no literal INFO total is invented. The terminal DBT cases use `--log=debug run --strict --base-env=minimal --epoch=2026-10-12T07:49:05+00:00` and their existing guest modes, without `--verify`; the other three terminal cases are native controls. They are not six canonical DBT runs. The procfs cases exercise ptrace assertions and are not an L2 cohort. No L3/L4, universal backend, full-validation, peak-stack or publication/landing claim is made.

Explicit goalpost assessment against H00591: (1) no assertion is weakened—the only product diff is three allocation boundaries in lib.rs; (2) no tolerance, exemption, ignored annotation, comparator or test selection policy changes—the focused selections are declared and their exclusions uncredited; (3) no failure is renamed as a pass—the before 0/2 exit 101, debugger startup exit 134 and uncommitted experiment remain distinct from clean-head successes; (4) no check is deleted—the original tests execute under their original stack/profile semantics and the requested additional checks all complete. Source and actual evidence satisfy the bounded review.

The final mechanism claim remains limited to the measured H00591-to-H6611 wrapper-storage reduction and unchanged-command repair. It does not assign historical good-to-bad byte growth to one D3 field or isolate it from accompanying Reverie integration, and no dynamic backtrace was obtained.


Final report bound: `validation-plain-main00591/report.md`, corrected SHA-256 `72993575944fba44de54655c0c96ab0e0baea408e607b9050b0742a304071adc`. The prior report hash `e6d8ac97352cb6cbd6cf1263522c9fe0822700d40206bbd31ae8a5607eac1aae` was superseded only to state the terminal fixture's explicit debug logging/strict/minimal-env/fixed-epoch flags and distinguish EOF default logging. No indexed result, source or test changed. The review is complete and clear for this exact bounded repair and evidence; no further test or source gate is requested.
