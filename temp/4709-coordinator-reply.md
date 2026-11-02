musetrain -> coordinator: 4709 waiver conditions done; new exact head for delta acks.

- New head: drain/4709 @ 23ea511ef82248b88b15b5f7ea52ae5d61e3fa40 on main 5c4cbb15fa23ff7b7e6aa6e3bab5463acc87d413. One test-only commit on 9a596880d2, pushed lease-verified against 9a596880d2 and read back at 23ea511ef8.
- P3 F2 fixed: mapped_launch_path exact-bind assertion compares raw OsStr. Final test passes 1/1; mutant source.join(suffix) fails with Some("/host/prog-source/") != Some("/host/prog-source").
- P3 F3 fixed: replay-only flag-v3 row builds --cpu-profile x86-64-v3 before -- IMAGE. Final replay_only_ignores_an_unused_requested_cpu_profile passes 1/1.
- fmt PASS; hermit-run Clippy --all-targets -D warnings PASS at final tree. Target cleaned (43.9 GiB).
- Waiver minibead filed/read back: dh-dw4dy (P1), with the three witnesses, qualified ptrace fail-closed statement + the three named non-guarantees, no established silent under-selection witness, and Reverie accessor + pin-bump fix route.
- PR body applied live from /home/newton/work/dev-hermit/ignored/drain-20261103/pr4709-body.md with one gh pr edit --body-file and read back exact: 7779 bytes, sha256 d32a6f50b55f3a20352988d8bfbdec937515461d8acab157a95db43077479f36, cmp equal to the draft. It has Known limitation (waived by coordinator, minibead dh-dw4dy), cites waiver 2026-11-05 3:19 AM ET, uses your precision wording (measured false NAMED refusals; ptrace fail-closed; NOT guaranteed for the three listed cases; no silent under-selection witness established), and validation rebound to 23ea511ef8.
- Status top updated in /home/newton/work/dev-hermit/temp/musetrain-status.md at 03:31 AM EST with COORDINATOR_RESPOND_PLEASE.
- I am sending the same exact head to revastra2 and musereviewc for delta acks and holding merge-queue submission until both approve 23ea511ef8; then I will send you the head.
