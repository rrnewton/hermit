# Hermit 2950 continuation

Registered slot: `hermit-ecx-complete`, agent `kvm-ratchet-cpuid`, generation 1.
Branch: `kvm-ratchet-hermit-ecx-complete`.

Reverie pull request https://github.com/rrnewton/reverie/pull/521 passed canonical validation and landed on main as `37e7b727a7f921d9c49bad4db6969297fc20f78a`. Its tree is byte-identical to reviewed head `dd07a830c047ed93edad450a7a1048fa04e43a16`.

Hermit pull request https://github.com/rrnewton/hermit/pull/2950 still points to old head `c3a85da26110fd8dfc31433c8d4109726127703b`, with a live changes-requested marker at that head. Do not treat that marker becoming stale after a push as resolution; the reviewer must explicitly confirm the fix at the new head.

This slot starts from Hermit main `89e684c8ce72e8cf0ded0cd30506f90b9570d9fd` and cherry-picks the original PR as `19912b9249f8c68b510a33bcbe280d85b4ac5c30`. Uncommitted work now:

- all 46 tracked Cargo revision entries point to landed Reverie `37e7b727`;
- the two active `ci/configure-build-jobs.sh` bindings and `ci/run-with-reverie-dbt-budget.sh` point to `37e7b727`;
- both DynamoRIO build inputs are byte-identical between `320412c5` and `37e7b727` (`build.rs` object `0ff8ae24b974`, vendor tree `a3c41e5d3630`), and both CI files record that carry;
- `cpuid_probe.c` now asserts zero for subleaf 1 of all advertised indexed leaves `0x04`, `0x07`, `0x0b`, and `0x0d`, while keeping success stdout unchanged.

Completed checks in this slot:

- canonical Reverie pin checker passed: 46 entries across 10 Cargo metadata files and all 3 DBT budget bindings agree on `37e7b727`;
- `cargo test -p hermit-manifest-plan`: 236 passed;
- fixture compiled with `cc -std=c11 -Wall -Wextra -Werror`;
- `clang-format --dry-run --Werror` passed;
- `git diff --check` passed.

An attempted `cargo test -p hermit-detcore` used the default parallel integration suite, produced broad unrelated environment failures, and left several long-running tests. Its process group was terminated cleanly. Do not cite it as product evidence. Next run the established focused library command, then the exact backend-parity cell through the official manifest runner under ptrace and KVM. After tests, commit in this slot, force-update PR 2950 with an exact old-tip lease, update the PR body, and request a fresh independent review and explicit resolution of the old finding.

The older `hermit-ecx-findings` slot is registered to dead agent `kvm-ratchet-hermit` and contains a duplicate uncommitted attempt. It was left untouched after the ownership mismatch was discovered. Do not reclaim it without the normal slot proof.
