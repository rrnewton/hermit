# HANDOFF — under_capture_the_kvm

Working slot: `/home/newton/work/dev-hermit/worktrees/slot132-kvm-stdin-hermit`

I was fixing the captured-stdin disagreement between ptrace and KVM. Reverie
had replaced explicitly configured KVM stdin with `/dev/null` whenever output
capture was enabled. Hermit also needed to replay a fresh, read-only snapshot
to each verification run rather than sharing one consumed file description.

Exact state:

- Hermit branch: `hermit-132/kvm-verify-stdin-replay`
- Hermit branch head: `de5a32be84de1fe9655841766880d33307dd5bab`
- Hermit base: `origin/main` at `26f5bc2de3d328b64c41c42ef093ecb9d89e98ab`
- Hermit remote ref was verified at the branch head above.
- Reverie pull request: https://github.com/rrnewton/reverie/pull/510
- Reverie landed main/merge SHA:
  `063fa37b05e562760f3d27c80ed8a6482b97b44a`
- Reverie feature commit:
  `a876574522ef3a77e682f7934982b6a5ce159e68`
- No validate is running; there is no validate handle or log path to recover.

Completed verification against Reverie `063fa37b`:

- Direct guest proof: ptrace and KVM both returned 0 and reproduced the same
  20 bytes; both outputs had SHA-256
  `009a2b837353ea6249d91dec38a26aa15afad1559d74ed358ac1d63598292afb`.
- Hermit library tests: 206 passed, 0 failed.
- `cargo clippy -p hermit --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `ci/run-with-reverie-dbt-budget-test.sh`: passed all cases.
- Reverie static ELF tests: 39 passed serially; library tests: 198 passed.

Next:

1. Verify the remote feature ref with `with-proxy git ls-remote origin
   refs/heads/hermit-132/kvm-verify-stdin-replay`.
2. Rerun `run_verify_replays_standard_input`,
   `run_verify_does_not_write_to_standard_input`, and `run_kvm_verify_*` at
   the final Hermit head. They passed before the final rebase; the sole new
   base commit is documentation-only, but the final-head rerun remains open.
3. Open the Hermit pull request, obtain exact-head admission/review, land from
   a clean detached worktree, verify remote content, and close TaskGraph task
   `under_capture_the_kvm`.

The output-capture sink work held by hermit-139 is adjacent and untouched.
No assertion, timeout, tolerance, comparator, skip, allowlist, or failure
classification was weakened.
