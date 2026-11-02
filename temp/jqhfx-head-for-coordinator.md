coord: dh-jqhfx is ready for you to submit.

PR: https://github.com/rrnewton/hermit/pull/5182
Branch: fix/dh-jqhfx-probe-lock
Exact head: f246147df3d9eaaa6460284605cd288e4eb53a99 (base 0220dd857a993be19f4d88a4cbf8d55f68249e1d, 2 commits; uncredentialed ls-remote read back exact).

Design: the lock holder publishes its passing XINUSE verdict beside the per-user /tmp lock (HostKey + probe options, same keying as the XDG cache); a waiter polls verdict + try-lock (180s), rechecks after acquiring, then queues on the blocking lock — no lock-timeout refusal for a live holder, so the second qualification succeeds with the first prober's verdict. Probe itself refuses after a cooperative 120s wall deadline. Lock open hardened (O_NOFOLLOW|O_NONBLOCK + regular-file). Hypervisor shares nothing and self-measures as before. Only hermit-cli/src/xstate_probe/mod.rs + 2 DAG files (count pins +5 composed: test.hermit_unit 1268->1273, on-host 1262->1267; validate.json regenerated).

Independently verified (separate verifier, not the implementer): diff exactly 3 files; push read-back; xstate_probe 25/25 incl. the 2 concurrent-qualification regressions; two cpu_profile_v3 CLI tests pass; concurrent pair via real CLI (ptrace, distinct fresh XDG caches, verdict absent at start) both exit 0 in 1.54s with one shared measurement; DAG --check canonical; manifest-plan validation_dag 43/43; fmt/clippy clean. Assurance: L0 + manual L1 pair (ptrace backend, default log, no relaxations); no L2 claim; full-profile receipt is the queue's.

Not landed; not submitted — yours to submit. 4709's real_random fix is in its final push step in parallel; I'll send that head separately.
