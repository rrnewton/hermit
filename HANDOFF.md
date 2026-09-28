# Completed wrkslots creation repair

The owner's goal is fulfilled: implementation, policy, both dependency gitlinks,
consumer inventory, and real registered slot demonstration are published.

Published dependency chain:

- agent-utils main: 14589e875c08e458d2f8af5a6f78b12128d5a367.
- Hermit main: ef48ca7e9dc91d53bed716f3ed519c6fcbd6dbdf, through
  https://github.com/rrnewton/hermit/pull/3012.
- Parent recorded Hermit gitlink landed in
  5e05bdb419e989181c717557043ab62c013ab369. The complete final report is on parent
  main in 2b2490c153e0b7d6a1f75f4e2e7e461f028c5f0f:
  https://github.com/rrnewton/dev-hermit/blob/2b2490c153e0b7d6a1f75f4e2e7e461f028c5f0f/ai_docs/wrkslots-consumers-20260914.md

The changed objects are Hermit's nested agent-utils gitlink and the parent's
recorded Hermit gitlink. The Cargo manifest Reverie revision remains
8c8c0a57649c9ffbf8a7a14291a64320f64b935f. Remote content readback verified all three
repositories, the final report and policy, and the PR body.

The owner explicitly authorized speculative soft-green landing without an
exact-head qualifying receipt. The PR body records the unrelated current-main
Rust-script build OOM and registration audit failure, the reviewed provider
change and passing checks, and the verified demonstration. No failed check or
missing receipt is represented as passing.

Exact landed-SHA post-land verification is armed through the persistent user
systemd timer wrkslots-post-land-20260914.timer. The first real check returned
WAITING because fresh main has no qualifying result. Its durable request, source,
state, tests, and installed-unit evidence are under the shared parent's
ignored/wrkslots-post-land-20260914/. It checks every 15 minutes. After main is
green, historical frozen validation still preserves the exact landed tree and
any remaining failures; its result is explicitly nonqualifying. Read state.json
and systemctl for the current status. Do not remove that directory while pending.

This slot remains registered to the demonstration's persistent Python owner
PID 287750, start ticks 157538416, with coordinator PID 2073905, start ticks
157042304. The owner is in a separate tmux process tree. The original CLI refused
its correct live coordinator; the fixed ordinary launcher created this slot in
33.438 seconds with internal proxy setup, real GitHub fetch, recursive submodules,
Git registration, ACTIVE generation 1, and event 6225 verified. Full provider
validation passed 3,367 Python tests and 871 Rust tests. Evidence remains under
/tmp/wrkslots-demo-20260914/. A separate Codex harness attempt failed with HTTP403
before any model response and is not counted as a successful harness run.

The persistent owner still waits for /tmp/wrkslots-demo-20260914/stop-owner.
Do not stop it or reclaim this slot merely because the task is complete or this
handoff exists. Follow normal live-process and handoff checks. The shared parent
contains other agents' unrelated changes; never commit, reset or clean it.

Consumers of this untracked HANDOFF.md: the successor coordinator or worker reads
it for the current state; wrkslots read-handoff and removal/recovery safeguards
consume its contents or digest. It must not enter the dependency PR.
