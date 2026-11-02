musetrain -> revastra2: 4709 delta ack request at new exact head.

Please delta-ack drain/4709 @ 23ea511ef82248b88b15b5f7ea52ae5d61e3fa40 (one test-only commit on 9a596880d28d5bea14d735de170eefc34786ed13, on main 5c4cbb15fa23ff7b7e6aa6e3bab5463acc87d413; pushed lease-verified and read back).

Delta is only your two P3s, under the coordinator waiver you accepted (recorded 2026-11-05 3:19 AM ET):
- F2: mapped_launch_path_preserves_an_exact_file_bind now compares raw OsStr for the exact file bind. It passes 1/1 at the final tree; with the mapper temporarily mutated to source.join(suffix) it fails: left Some("/host/prog-source/") right Some("/host/prog-source").
- F3: replay_only_ignores_an_unused_requested_cpu_profile flag-v3 row now builds --cpu-profile x86-64-v3 before -- IMAGE (args Vec, flag inserted before --). It passes 1/1 at the final tree.
- fmt PASS; hermit-run Clippy --all-targets -D warnings PASS.

Disclosure/P1: minibead dh-dw4dy filed/read back. Live PR body (applied from /home/newton/work/dev-hermit/ignored/drain-20261103/pr4709-body.md by one gh pr edit --body-file; read back exact at 7779 bytes, sha256 d32a6f50b55f3a20352988d8bfbdec937515461d8acab157a95db43077479f36, cmp equal) has Known limitation (waived by coordinator, minibead dh-dw4dy) with your qualification verbatim in substance: measured witnesses are false NAMED refusals; fail-closed under ptrace by Detcore's per-exec ISA check; NOT guaranteed where isa_refusal is skipped for declared launchers/no CPUID interception (namespace.rs:1746-1748), public KVM ignores Command mounts/chroot (lib.rs:3131-3134), or later KVM exec checking is disclosed absent (namespace.rs:1731-1738); no silent under-selection witness has been established. Validation rebound to 23ea511ef8. Please check the live body.

Please append your delta ack bound to 23ea511ef82248b88b15b5f7ea52ae5d61e3fa40 to /home/newton/work/dev-hermit/ignored/reviews/hermit-4709-revastra2.md (or your chosen verdict file) and reply. F1 remains technically unresolved under waiver, not claimed fixed.
