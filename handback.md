# Ptrace output-future stack repair

Ready for hand-back: one clean commit fixes the two original ptrace procfs regressions without changing their tests or stack settings. Each test passed in all three independent DEBUG Nextest rounds at exact H6611. All agreed runtime, lint, documentation and formatting checks are complete and green; the unchanged Reverie pin is clean. No publication or landing is claimed.

| Item | Exact identity |
| --- | --- |
| Hermit fix | `6611e75333eba5682a21aa9b5e54444eb32747dc` |
| Requested Hermit base | `00591c8a43e5d25bc50547ad568184eceb41f417` |
| Unchanged Reverie pin | `16ffab7c558c47c78c43aa8e0e4242eb2deeeb3d` |
| Reported bisect good | `2a6b3c87e25d8917fee58686fd56c57cfee81639` |
| Reported bisect bad | `9f57cc36bf13e0484be50e141384be6f3bd9996d` |

Source and local fetch provider: `/home/newton/work/dev-hermit/worktrees/slots/dbt-lane-20261010/ignored/d3-procfs-stack-v1/hermit-main00591`. The fix is one commit directly on the requested base, with only `hermit-cli/src/lib.rs` changed (22 additions, 30 removals). The staged patch SHA256 is `dbd8b0efa1c7188715901a3b1c8bf2298e5f04aa203e56a56fc44db85be182e9`. The author and committer are `hermit-bots <341730520+hermit-bots@users.noreply.github.com>`; the first commit-body line is the freshly obtained disclosure. No publication or landing is claimed. The parked statistics pair H342daf/Rff30 was not changed.

## Repair and attribution

The shared output-dispatch future is heap-owned before the retained ptrace operation or deadline future captures its backend state. This applies to both ptrace output entry points and the common non-ptrace output helper, matching the existing status-dispatch heap boundary. Ptrace construction remains after admission; polling, cancellation and drop remain on the same thread. Skid reporting, errors, deadlines and Linux/Detcore semantics are unchanged. The tests, features, stack size, assertions and inherited-input provenance are unchanged. This is a layout repair, not a new scheduling policy or backend exception.

The bounded source/callsite review found no new recursion. The exact retained test binary shows large nested async frames, and the single controlled source diff both reduced the relevant wrapper frames and changed the unchanged two-test Cargo result from 0/2 to 2/2. The installed GDB 9 aborted internally before test execution; no dynamic backtrace or precise simultaneous stack-total claim is made.

| Individual compiled frame | Before bytes | After bytes |
| --- | ---: | ---: |
| Retained ptrace operation entry | 73,352 | 6,232 |
| Output/skid wrapper poll | 69,512 | 36,008 |
| Non-ptrace output constructor | 105,272 | 4,584 |
| Non-ptrace output poll | 171,000 | 35,592 |
| Original procfs test | 8,456 | 8,456 |
| Deep output dispatch poll | 254,472 | 254,472 |
| Ptrace syscall-event poll | 565,672 | 565,672 |

These are individual x86 prologues, not an additive live-stack measurement. Exact ELF hashes and addresses are in [frame-comparison.json](frame-comparison.json). The before ELF SHA256 is `b75f91a8d8291bd97dafd1ce75d7be07be0e70f66411b185611ca7b76b52230e`; the first patched ELF is `cd7c0411ff161efd0be3b6292ccfe4a76523a1351031af4f1b0a7beec67d3008`.

## Validation boundaries

The coordinator reports the original bisect/repetitions using DEBUG Nextest, unoptimized with debuginfo, with `third-party-backends,kvm-execution-tests`. The lane's retained before/first experiment instead used default-profile Cargo test on devbig014:

```text
cargo test --offline --locked -p hermit --features third-party-backends,kvm-execution-tests --test procfs_determinism chroot_mountinfo_subset_keeps_fdinfo_identity_consistent -- --nocapture
```

That selects both the normal and `_with_readonly_proc` cases. Before: 0/2, exit 101, stack overflow in the private-namespace libtest child. First patch experiment: 2/2, exit 0. Three subsequent independent rounds at clean H6611 also passed both tests in every round. Each Cargo round filtered 49 tests and ignored none. No stack/thread override or added relaxation was used. Matching-runner final acceptance is complete: three independent DEBUG Nextest rounds at H6611 each passed both tests (2/2, 49 outside the selection, exit 0), with the actual unoptimized/debuginfo profile and no stack-setting change. These results are retained as `head-nextest-debug-r1`, `r2` and `r3` under the validation directory. The default Hermit lib/bin suite also passed 896/896 with no skips; the DBT-enabled lib/bin suite passed 936/936 with no skips.

The closed exact-head checks are:

| Check | Actual result |
| --- | --- |
| DEBUG Nextest, the two original procfs tests | Three independent rounds, 2/2 each |
| Default Hermit lib/bins | 896/896, no skips |
| DBT-feature Hermit lib/bins | 936/936, no skips |
| Existing live/saved/offline CLI selection | 11/11, 407 outside selection |
| Inherited-terminal target | 6/6: three DBT cases with `--log=debug` and three native controls |
| Workspace/all-targets/all-features Clippy with `-D warnings` | Exit 0 |
| Hermit all-features rustdoc with `-D warnings` | Exit 0 |
| Workspace formatting | Exit 0 |

The regression acceptance uses DEBUG/unoptimized test profile; the broader runtime suites use the maintained validate profile. The actual CLI, Rust runtime and Release native client are joined to R16ff in `production-linkage.json`. The unchanged stdin EOF fixture uses default logging and still asserts strict canonical matched/bitwise parity and nonzero INFO evidence, without inventing a retained INFO total. The terminal DBT fixtures retain strict mode, a minimal base environment and their fixed epoch in addition to debug logging. These are focused checks; no full validate, new census, canonical guarantee for the ptrace fixtures, or flake-rate estimate is claimed.

The closed [executor report](validation-plain-main00591/report.md) has SHA256 `72993575944fba44de54655c0c96ab0e0baea408e607b9050b0742a304071adc`; it links the full literal commands and raw results. All commands are terminated. Its `source-after.json` records the clean final source and pin; the source worker independently rechecked clean H6611, exact parent00591 and checked-out R16ff after executor release. The immutable before/experiment artifacts remain under [validation-plain-main00591](validation-plain-main00591). [source-investigation.md](source-investigation.md) retains the ordered investigation, and [committed-source.json](committed-source.json) binds the exact clean commit and patch.

## Resource qualification

Execution used nice level 19 with affinity CPUs 0–39 and 40 Cargo/CMake jobs, through the agreed plain Cargo route. The retained prebuild free space was 604,055,433,216 bytes. Subtracting the conservative 64 GiB estimate (68,719,476,736 bytes) left a projected 535,335,956,480 bytes, above the 450 GB launch requirement. Closing boundary free space was 568,853,413,888 bytes. Actual task-attributable incremental footprint, peak memory and peak stack were not measured; the host free-space delta is not attributed to this task. The 400 GB live floor is a policy bound, not a claim of continuous monitoring. No new probe or harness was added for this hand-back.

The original DEBUG fixture failures, debugger startup failure, controlled dirty-source success and committed-head repetitions remain separately attributed. The historical D3 bisect is coordinator evidence; this investigation does not measure the exact historical good-to-bad byte growth or claim a new local bisect. The final [independent source/frame/acceptance review](source-review.md), SHA256 `b83a10d524c5b2411fa28931d00f8f991650eb6e3a7fb6ff772a2969ca906f01`, joins the corrected executor report, exact clean source, requested DEBUG acceptance and separately scoped finish results. It found no lowered assertion or other goalpost change. This storage-only change does not introduce any numbered syscall, Reverie abstraction, determinization or scheduling-policy review trigger; the earlier D3 provenance change remains separately reviewed.
