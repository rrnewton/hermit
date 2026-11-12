Small follow-up on PR 5182 (head f246147df3d9eaaa6460284605cd288e4eb53a99, body currently 3485 bytes). Owner rule 5815: the PR body must have a Test plan section. Independent check found headings: who-am-i tag, Owner, Plain Language Summary and Project Impact, Determinism, Linux Semantics, Validation — but NO "Test plan" heading anywhere.

Do this (body-only edit, no code change, no new commit):
1. Fetch the live body via herdr-run: `with-proxy gh pr view 5182 --repo rrnewton/hermit --json body` (exact paced form that worked).
2. Insert the following section after **Validation** (keep every existing byte otherwise; keep total body well under 8 KB):

**Test Plan**

- `cargo nextest run -p hermit-run --lib -E 'test(xstate_probe)'` — 25/25 passed, including 2 concurrent-qualification regressions.
- `cargo nextest run -p hermit-run --test cli` (`cpu_profile_v3`) — 2/2 passed.
- Concurrent ptrace CLI pair with distinct XDG caches (shared verdict absent at start): both exit 0 in 1.54s, one shared measurement.
- `generate-validation-dag --check` — `ci/dag/validate.json` canonical.
- `cargo test -p hermit-manifest-plan validation_dag` — 43/43 passed.
- `cargo fmt --all -- --check` clean; `cargo clippy -p hermit-run --all-targets -- -D warnings` clean.

3. Apply with one paced `gh pr edit 5182 --repo rrnewton/hermit --body-file <file>` via herdr-run, then read back: confirm a "Test Plan" heading exists, body bytes, and headRefOid still f246147df3d9eaaa6460284605cd288e4eb53a99.
4. Report DONE with new body byte count + confirmation. Do not touch code or the branch.
