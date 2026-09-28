# Deterministic vectored-I/O retry handoff

## Result

- Starting head: `67d6f3ea8e36222e42db966c08645642ce68801a`
- Implementation commit: `f8bc4909ffb84277ebd51817fc173b9aba1f13a2` (`Fix deterministic vectored I/O retries`)
- Gate-repair commit / final head: `6c90880ebebcfff80478b555c05f500d6e307716` (`Make KVM vectored gate dependency-explicit`)
- Final diff SHA-256 from the starting head: `6a3155879ec538d4e2dbc20235cb2d85c00eea8293a6a29648453cd6066e5248`
- Gate-repair-only diff SHA-256: `cbbff30535726ad3b3b0095bc1e631cf036d6237a0fc6d50388e93a057c90893`
- Branch: `codex/kvm-detcore-vectored`
- No push or merge was performed.

The commit gives logically blocking `readv`/`writev` and current-position `preadv2`/`pwritev2` on internally nonblocking pipe/socket/eventfd descriptors one imported iovec snapshot for all retries and post-completion I/O evidence. It preserves the one-shot `RWF_NOWAIT` path, syscall identity and flags, open-file replacement checks, pipe atomicity/partial progress, and direct explicit-offset operations. Signal wakeups now use restartable I/O resources and target disposition handling for scalar and vectored partial writes. The record-mode regression now performs strict canonical replay verification.

## Validation

- `cargo test -p hermit --test preadv2_pwritev2_pipe -- --list`: all 5 tests are listed, including `kvm_current_position_vectored_descriptor_matrix_requires_pr529_and_pr538`.
- `cargo test -p hermit --test preadv2_pwritev2_pipe`: 4 passed, 0 failed, 1 ignored with reason `requires combined PR529 per-thread scratch + PR538 vectored support`. The four default ptrace tests exercise the full pipe/socket/eventfd matrix, strict A/B extent/digest assertions, poison rejection, current-position syscall identity/flags, descriptor replacement, `RWF_NOWAIT`, partial progress, signals, and canonical record/replay.
- `cargo test -p hermit --test preadv2_pwritev2_pipe kvm_current_position_vectored_descriptor_matrix_requires_pr529_and_pr538 -- --ignored --exact --nocapture`: expected failure on the current pin, cargo rc 101 in 0.30 seconds, with `readv pipe snapshot failed: count=-1 errno=14`; 0 passed, 1 failed, 0 ignored.
- `cargo test -p hermit --test writev_determinism`: 2 passed, 0 failed. The new non-root sibling-signal regression returned exact scalar/writev partial counts `4096,4096` under ptrace.
- Native guest coverage emitted all 10 descriptor-matrix markers and exact pwritev2 partial count 4096.
- `cargo test -p hermit-detcore`: 703 passed, 0 failed: 654 unit + 32 misc + 17 integration; empty binary/doc groups also passed.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `git diff --check`: passed.

One preceding full-package attempt ended abruptly during the 32-test misc harness with cargo rc 101 and no panic, failed assertion, signal report, or normal test summary. The exact misc target immediately passed 32/32, and the complete package rerun passed all 703 tests.

## Defect-restoring mutations

Each mutation failed the intended assertion and was restored before the final gates:

1. Restricting the read retry route back to pipes made the socket case return physical `EAGAIN`.
2. Removing the saved iovec from evidence logging made strict evidence report the poison address/zero digest instead of the original A extent/digest.
3. Removing `ERESTARTSYS` from the shared polling resource reproduced the signal hang: timeout after 30.84 seconds and 68,734 retries.
4. Removing `--verify`, `--verify-strict`, and the verification report made the record test fail because it saw recording completion but no replay success.

## KVM boundary (not a pass)

- Pinned Reverie is `8c8c0a57649c9ffbf8a7a14291a64320f64b935f`; it does not implement `preadv2`/`pwritev2` and retains the shared scratch-lifetime defect. The committed KVM matrix is therefore an explicit ignored integration test, not a passing runtime skip. It contains no capability probe or early return.
- Combined integration lane command after composing PR529 and PR538 without moving either source branch: `cargo test -p hermit --test preadv2_pwritev2_pipe kvm_current_position_vectored_descriptor_matrix_requires_pr529_and_pr538 -- --ignored --exact --nocapture`.
- Read-only PR538 worktree `/home/newton/work/dev-hermit/worktrees/slots/kvm-vectored-io` was clean apart from its pre-existing `ignored/` and remained untouched at `3646ba2c662f65b97e94d39f60852b62610ca5a0`.
- Pairing this Hermit tree with PR538 still failed: full matrix `readv` pipe returned `-1/EFAULT`; safehermit run `20260909T215549Z-4161442` reached blocking `preadv2` and returned `-1/EFAULT` after release. Trace `20260909T214924Z-3937179` showed valid retry scratch `0xde00` and original targets `0x3fffeb64`/`0x3fffeb68` before EFAULT.
- A supported scalar/writev KVM sibling-signal probe was schedule-sensitive: safehermit run `20260909T221915Z-768174` completed, while the trace-enabled cargo regression timed out at 30.23 seconds after non-root dtid 4 returned `EFAULT` from `writev(6, 0x1b50e20, 2)` and then faulted with vector 14 at `0x116ea2000`. The exploratory failing Rust test was removed; no existing assertion was weakened.
- Cause: Reverie `runtime.rs::run_static_elf_process_with_tool` exposes tool scratch before each thread handler but unconditionally hides it afterward. Threads share `self.memory.clone()`, so a sibling can globally unmap the user-access page while the root still owns a live `KvmStackGuard`. Guard drop only clears a per-vCPU `checked_out` bit; there is no shared exposure refcount.
- PR529 commit `c632c111619cb47922a72235b3e1130b91355603` repairs this exact lifetime class using per-thread scratch pages/paired transport slots and simultaneous committed-guard tests; it is more than a panic guard. PR538 does not contain PR529 (`git merge-base --is-ancestor c632c111 3646ba2c` returned 1). No branches were combined.

## Cleanup

- Removed only the two explicitly authorized orphan reviewer process trees rooted at PIDs 1332961 and 1654266 and verified those exact roots were gone.
- Removed only this task's disposable overlay worktrees `/tmp/hermit-pr538-overlay` and `/home/newton/work/dev-hermit/ignored/hermit-pr538-overlay`, plus the 3.9 GB disposable target `/home/newton/work/dev-hermit/ignored/pr538-vectored-target`.
- The original Hermit and Reverie slot worktrees and unrelated terminals/processes were not modified.
