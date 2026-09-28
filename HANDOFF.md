Task: kvm-lane-to-full-determinism-and-parity
Worker: /root/kvm_queue_delta
Parent: /root; live TaskGraph owner Ensure deterministic KVM parity | dev-hermit
Slot: kvm-raw-stderr-20260917, generation 1, assigned agent kvm_queue_delta
Coordinator/owner PID: 1210520
Branch: codex/kvm-raw-stderr-20260917
Starting HEAD: 32033aac48055103305b75d5619cbb895e87d81c
Starting tree: 7b9246a62f8ef3babf08d51b7207449c91ea4a03

2026-09-17 12:10 UTC. Root created this registered slot normally through wrkslots. Initial status was completely clean and HANDOFF.md was absent. No product edit, formatter, compiler, test, guest or public action has occurred in this slot. Root has released preparation/HANDOFF writes only. Product edits and execution await root's concrete release after reading the prepared patch and plan. No new PR while the preceding capture change is unlanded.

Prepared files:
/home/newton/work/dev-hermit/worktrees/slots/kvm-parent-reader-support-20260916/ignored/kvm-queue-delta-20260917-1157/raw-stderr-preparation/

raw-stderr.patch SHA256 f091820a691f6bcc7f7da68b3e8a77c60040d402518c10ba2cab1bacd7d4929f
PLAN.md SHA256 399b94e8ad1e51f69de51b0816f7a3cb3dd705c9bae4bae309c89d755bbf8330
VERIFICATION-PLAN.json SHA256 1f66139ac096f89dfa869b67d98b27c3b8de37b33f792423bd7b27f3a4ffd4ac
MANIFEST.json SHA256 4aff00553d300c09d4efe127a98472e6c91dc02b976e1b4a52548cc5ac39a2b9
All 35 retained artifact files were rehashed and matched at 12:10 UTC.

The patch changes only scripts/cross-backend-detlog-diff.rs: raw stderr remains bytes; only failure diagnostics use lossy text; authoritative UTF-8/canonical validation, status checks, backend refusals, existing assertions, comparator and main remain. A production finish_capture helper supports native filesystem controls without a guest. Planned successful self-test count is 52 (26 existing plus 26 new), not an executed result. Two prepared mutation patches must fail the new controls if later run. The artifact is not yet formatted, compiled or independently reviewed.

Do not execute prior-bounded-native-caller.py unchanged: it is retained reference only, hard-codes the wrong source/selector and has an unbounded final proc.wait(). The later exact-source caller must use externally bounded/reaped machinery and bind its own hash, scope, output and completed accounting. ready_to_execute remains false.

This is one donor group from https://github.com/rrnewton/hermit/pull/2958 in the linear continuation of https://github.com/rrnewton/hermit/pull/2969. It does not close either broad proposal or establish guest-output equality, canonical backend parity or a new census result. Preserve all prior evidence, including the first artifact-check refusal described in PREPARATION-REFUSAL.md.

2026-09-17 12:23 UTC continuation. Root released one-file implementation. The approved patch is applied and formatted; mode 0755 and all other tracked paths are preserved. No compilation/test/mutation/publication ran. The actual source/caller/plan and complete launch argv are in ignored/raw-stderr-implementation/REVIEW-REQUEST.md and SOURCE-CALLER-BINDING.json. Caller and all 1,726 source entries are bound; final approval files do not exist. Capture has landed at 7f84482cd88a9fd5701220b200b30e734ffb72db with the same whole tree as32033aac; root instructed preserving the already-dirty source, committing later, then ordinary rebase before final source review/publication. Do not stash or restore.

2026-09-17: Raw stderr landed and next proposal prepared.

https://github.com/rrnewton/hermit/pull/3068 was merged by ordinary rebase at dc3a873b22bfb0a7c3b9577b91c1d4c9e364a37e. Reviewed c01b0e11b2121f8486312d1a2f19ed24e477e52d and landed commit have identical full tree 437938d28cb9e89bc10ad9eb23bdf588c5fe6f31 and patch adf835ab05c5780bb7845b31d22088188d6c2582c7f401da6867668639dfc387. Original author and complete message are preserved. Product tracked work/index is clean; local branch remains at c01b0e11, origin/main readback at dc3a873. Do not change refs or discard evidence merely to make the local branch resemble its rebased server identity.

Both actual native and Claude reviews are published and byte-verified in ignored/raw-stderr-publication. Candidate passed 52 native checks; both causal mutations failed the unchanged controls. Full source, caller, refusal, success, mutation, cache-correction, rebase and inactive-scope records remain under ignored/raw-stderr-implementation, ignored/raw-stderr-native-v2, ignored/raw-stderr-review, ignored/raw-stderr-review-addendum and ignored/raw-stderr-publication. Retain the private external cache /tmp/kvm-raw-stderr-rust-script-y9s1ntlr. Parent independently read the complete landing and recorded verified TaskGraph note 27045.

Actual normal pre-push hook inspection confirms cargo clippy --workspace --all-targets -- -D warnings ran through the installed dispatcher and the tracked quiet checker. Successful Clippy output was intentionally discarded by that checker, so there is no separate raw Clippy log or timing. Corrected PR body was read back byte-identically; earlier native report and commit text remain unchanged. This hook result does not create a full validation or backend parity claim.

Next authorized work is preparation only: ignored/staged-summary-preparation/PROPOSAL.md, SHA256 79f99f1b11952fd3e17fcb0163cfd1ca3304b2bcf7d35bb3a2202322a7509d99; source-binding.json SHA256 7ab4f03a9bccff2c970a9b49fede00e1ad69c83ffec387bba86ac77173556526; READBACK.json SHA256 87ff7869d9a6cd8f648544363ebd52b015dc66e456303ba2adf72134e3a48ca8. All 22 source/history copies verified, exact landed base dc3a873. Root has the proposal for a concrete namespace/descriptor/control read; implementation and execution are NOT yet released.

Selected future scope: capture-only 1 MiB read/publication limit, with the existing temporary writer and serialization explicitly unbounded. Preserve original final path, post-mount/chroot collision checking and publication before capture.finish; carry held private storage across the fork. Full streaming-writer alternative and DBT's absent summary transport remain recorded, not implemented. No new product edit, test, build, guest or PR occurred for this proposal. Keep https://github.com/rrnewton/hermit/pull/2958, https://github.com/rrnewton/hermit/pull/2969 and the broad KVM task open.
