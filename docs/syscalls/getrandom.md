---
schema: syscall-pages/v1
syscall: getrandom
nr_x86_64: 318
linux_determinism: nondeterministic
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getrandom&body=Discussion+for+the+%60getrandom%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetrandom.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getrandom%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 5
---

## Summary

`getrandom(buf, buflen, flags)` fills a buffer with bytes from the kernel's cryptographic random number generator. Detcore handles it (`detcore/src/lib.rs:2538`, `detcore/src/syscalls/misc.rs:552`): it keeps the kernel's flag, length and fault rules, but the bytes come from the calling thread's deterministic PRNG, seeded from `--rng-seed` (or `--seed`), so the same run produces the same bytes.

Manual reference: Linux man-pages getrandom(2), random(7).

## Linux semantics

Since Linux 5.6 every source flag draws from the same ChaCha20-based generator. The kernel checks `flags` first: undefined bits, or `GRND_INSECURE` together with `GRND_RANDOM`, give `EINVAL`. The flags argument is an `unsigned int`, so bits above 31 in the register are dropped before the check.

If the generator is not yet initialized (only possible early in boot), a call without `GRND_INSECURE` waits for it, or returns `EAGAIN` with `GRND_NONBLOCK`, and a signal during that wait gives `EINTR`. Once initialized, no call ever waits.

The length is clamped to `MAX_RW_COUNT` (`INT_MAX` rounded down to a page). A length of 0 returns 0 without touching `buf`. The kernel copies in chunks; if a copy faults after some bytes were written it returns the count so far, and `EFAULT` only when nothing was written. Between chunks of a large request a pending signal ends the call early with a short count.

On Linux 6.11 and later, the vDSO exports `__vdso_getrandom`, which glibc 2.41 and later uses for `getrandom(3)`: it keeps per-thread ChaCha20 state in user memory, keyed by one real `getrandom` syscall, and re-keys whenever the kernel's generator moves on.

### Flags and options

- F1: `flags` 0: fill `buf` from the generator, waiting first if it is not initialized.
- F2: `GRND_NONBLOCK` (1): return `EAGAIN` instead of waiting when the generator is not initialized.
- F3: `GRND_RANDOM` (2): historically the blocking pool. Since 5.6 it draws from the same generator as F1; with `GRND_NONBLOCK` it still returns `EAGAIN` before initialization.
- F4: `GRND_INSECURE` (4, Linux 5.6 and later): never wait, even before initialization.
- F5: `GRND_RANDOM|GRND_INSECURE`, or any undefined bit in the low 32 bits: `EINVAL`, before `buf` is touched. Bits above 31 are ignored.
- F6: `buflen` 0: returns 0, whatever `buf` is, including NULL.
- F7: `buf` NULL or unmapped with nonzero `buflen`: `EFAULT`.
- F8: a buffer that becomes unwritable part-way: returns the number of bytes written before the fault.
- F9: `buflen` above `MAX_RW_COUNT` (`0x7ffff000` on x86_64): clamped, so at most that many bytes are returned.
- F10: a signal pending during a large request returns a short count, or `EINTR` while waiting for initialization.
- F11: the vDSO path `__vdso_getrandom`, which produces bytes in user space and re-keys at host-dependent moments.
- F12: per-thread and per-process streams: every thread and every child after `fork` or `clone` gets independent bytes.

## Determinism analysis

`linux_determinism: nondeterministic`. The output is drawn from host entropy and is meant to be unpredictable; no two calls repeat. The secondary sources are the generator's initialization state (F1-F4), signal timing for large requests (F10), and the re-keying moments of the vDSO path (F11), which depend on host time.

### Nondeterminism sources

- Host entropy, by design
- Whether the host generator is initialized at the time of the call
- Signal arrival during a large request
- vDSO re-keying, which follows the kernel's generator generation and therefore host time

## Hermit design

Target policy: `emulate`. Detcore answers with synthetic bytes from a seeded PRNG instead of host entropy. `handle_getrandom` (`detcore/src/syscalls/misc.rs:552-561`) passes the call to `crate::random::getrandom` (`detcore/src/random.rs:354-367`) with the calling thread's PRNG (`thread_prng`, `detcore/src/tool_local.rs:2689`):

- F5: flags are cut to 32 bits and checked first (`validate_getrandom_flags`, `detcore/src/random.rs:240-250`). `GRND_NONBLOCK`, `GRND_RANDOM` and `GRND_INSECURE` are allowed, and the `RANDOM|INSECURE` pair and every other bit give `EINVAL`.
- F1-F4: every allowed flag combination draws from the same stream, which is always ready, so Hermit never waits and never returns `EAGAIN`. That matches Linux on any initialized host.
- F9, F6, F7: the length is clamped to `GETRANDOM_MAX_BYTES`, `(i32::MAX) & !4095` (`detcore/src/random.rs:252-257`); a length of 0 returns 0 before `buf` is examined; a NULL `buf` is `EFAULT` (`detcore/src/random.rs:362-366`).
- F8: `fill_bytes` (`detcore/src/random.rs:293-351`) writes 4096-byte chunks through a page-protection-respecting path (`write_random_chunk`, `detcore/src/random.rs:259`). After a partial write it returns the count so far, and `EFAULT` only if nothing was written. A copy failure other than `EFAULT` is not a guest error: `copy_error` (`detcore/src/random.rs:64-69`) turns it into a tool error and the run stops.
- F10: Detcore fills the whole request in one handler call, so a pending signal never shortens it and `EINTR` cannot happen. A guest cannot tell this from a run where the signal arrived after the call.
- F12: the root thread's PRNG is seeded from `rng_seed()` (`detcore/src/tool_local.rs:2259`), which is `--rng-seed` or else `--seed` (`detcore-model/src/config.rs:1311-1313`). Each new thread's PRNG is derived from its parent's and from a deterministic identity supplied by the backend (`reseed_child_rngs`, `detcore/src/tool_local.rs:2065-2068`), so threads and children get distinct streams that repeat from run to run.
- F11: on ptrace, the pinned Reverie submodule (d766df20) replaces `__vdso_getrandom` with a stub (`reverie-ptrace/src/vdso.rs:83-121` and `:280`). The stub's parameter query returns `ENOSYS`, so glibc 2.41 and later fall back to the syscall for every `getrandom(3)`, and any other call becomes the real `getrandom` syscall, which Detcore then answers. The cell `c-programs/random-sources-root-only` records the effect: its vDSO leg prints "vdso-getrandom unavailable".

Why `emulate` and not the alternatives:

- `passthrough` hands the guest host entropy, which is the nondeterminism.
- `model` would fit if the guest could set or read back generator state. It cannot: the PRNG is configured by the run's seed, not by the guest, and the guest sees it only through the bytes it is given. That is an emulated answer from a deterministic source, like the fixed answers of `uname` and `get_mempolicy`.
- `refuse` would break almost every program, since glibc, OpenSSL, Python and Rust's standard library all depend on it.

The bytes are not cryptographically secure under Hermit: `thread_prng` is a `Pcg64Mcg`, and its doc comment says it "should not be used for crypto" (`detcore/src/tool_local.rs:2687-2688`). That is inherent to deterministic execution and is the intended trade.

The code tags `TODO-HUMAN-REVIEW(#545)` at `detcore/src/random.rs:239` (https://github.com/rrnewton/hermit/pull/545) and `TODO-HUMAN-REVIEW(PR-1060)` at `detcore/src/tool_local.rs:2062` (https://github.com/rrnewton/hermit/pull/1060) ask for human review of the flag, stream and fault semantics and of child reseeding.

`linux_determinism` stays `nondeterministic`: Linux's answer is random by design, and Hermit substitutes a seeded stream.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6): with raw `syscall(SYS_getrandom, ...)` under `hermit run --verify` on ptrace, check that flags 0, `GRND_NONBLOCK`, `GRND_RANDOM`, `GRND_NONBLOCK|GRND_RANDOM` and `GRND_INSECURE` each return the full length; that bit 32 alone is ignored; that bit 31, bit 31 with bit 32, and `GRND_RANDOM|GRND_INSECURE` give `EINVAL`; and that `getrandom(NULL, 0, 0)` returns 0.
  Existing candidate: `check_getrandom_flags` in `tests/c/random_sources.c:52-107` (cell `c-programs/random-sources`, `tests/e2e/manifests/c-programs.yaml:6788`, verify ci on ptrace only). It does not try `GRND_INSECURE` alone.
- T2 (covers F7, F8, F9): `EFAULT` for `buf` `(void *)1` with `SIZE_MAX` (this also exercises the clamp), a short count of 8 for a 16-byte request that crosses into a `PROT_NONE` page, and a count of one page for a request of one page plus 8 bytes whose second chunk faults.
  Existing candidate: `check_getrandom_faults` in `tests/c/random_sources.c:109-144`, same cell. No test checks that a request above `MAX_RW_COUNT` on a large enough mapping returns exactly `0x7ffff000`.
- T3 (covers F12): sample `getrandom` from the root thread and from several pthreads, and require the bytes to be identical across `--verify` runs on ptrace and different between threads. Repeat across `fork` and check that parent and child differ.
  Existing candidates: the thread samples in `tests/c/random_sources.c:312-319` (cell `c-programs/random-sources`), the root-only stream in cell `c-programs/random-sources-root-only` (`tests/e2e/manifests/c-programs.yaml:17896`), and `getrandom_intercepted` at `detcore/tests/misc/mod.rs:1842`, which checks only that an 8-byte request returns 8, without comparing runs. No test covers the `fork` case.
- T4 (covers F11): resolve `__vdso_getrandom` and query its parameters under Hermit on ptrace. The query must decline, and glibc `getrandom(3)` must still give identical bytes across runs.
  Existing candidate: `fill_vdso_getrandom` in `tests/c/random_sources.c:249-301`, exercised by both random-sources cells.
- T5 (covers F10): send a guest a signal from another thread while it requests 64 MiB with a handler installed. Under Hermit the call must return the full length and never `EINTR`, identically across runs.
- T6 (covers F8): a copy failure that is not `EFAULT` (a real `process_vm_writev` permission failure) must stop the run with a tool error, not return an errno to the guest.
  Existing fixture: `hermit-cli/tests/fixtures/random_copy_fatal.c`, with a `getrandom` mode. No test at this SHA references this fixture by name, so it is not yet wired into a test.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy emulate (Linux flag, length and fault rules kept; bytes from the thread's PRNG seeded by --rng-seed or --seed, random.rs:354); linux_determinism nondeterministic (host entropy); 12 flag behaviours and 6 tests listed; fixture random_copy_fatal.c is not referenced by any test at this SHA; no coverage token recorded, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/random_sources.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getrandom`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getrandom&body=Discussion+for+the+%60getrandom%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetrandom.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getrandom%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `getrandom`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- `hermit-cli/tests/fixtures/random_copy_fatal.c` exercises the fatal non-`EFAULT` copy path for `getrandom` and `readv` of `/dev/urandom`, but no test, manifest or build file at this SHA names it. Either a test was lost or the fixture is dead.
