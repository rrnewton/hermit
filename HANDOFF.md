# HANDOFF — slot `chaos` (agent `hermit-chaos`)

Written at teardown, 2026-08-06. **Tree clean, everything pushed.**

---

## Task 1 (finished, awaiting landing): `chaos_seed_lists_are`

| | |
|---|---|
| Branch | `chaos-seeds-widen-to-measured-sample` |
| SHA | `21c15e1fba9e9043f2b9f72c5be585331e82d1ef` |
| Base | `origin/main` `4c70658e785834737cbe1524f77330c781a6f5ea` |
| PR | https://github.com/rrnewton/hermit/pull/1750 (draft, `MERGEABLE`) |
| Task state | `in_progress` + `implemented` |

Widened `determinism-stress/order-violation` chaos seeds from the hand-picked
pair `[0,9]` to the contiguous prefix `0..31`. Nothing relaxed — `min_distinct`,
`min_passes`, `min_failures`, `--strict`, `--chaos` all byte-identical.

**Next step:** lander picks it up. No exact-head validate receipt was produced
(manifest-only change); the merge gate owns that.

**Note for whoever lands it:** the shipped `[0,9]` is *already red* at
`4c70658e` (`distinct=1 passes=2 failures=0`) — reproduced directly. That is
under-sampling, not a determinism regression. Chaos exploration is healthy:
`distinct=2` at every prefix width 2..64.

---

## Task 2 (WIP, do NOT tag implemented): `boolean-blindness-is-a-46-of-73-class-not-7-fixtures`

| | |
|---|---|
| Branch | `fix/parity-fixtures-emit-observed-value` (**extends PR #1719 — do not open a competing PR**) |
| SHA | `e1bb1cac2d17a5717750a25e2685535fd9d0b106` |
| Base | `origin/main` `4c70658e…` (up to date, no rebase needed) |
| PR | https://github.com/rrnewton/hermit/pull/1719 |
| Started from | `492e78acd277c5e0e0bda033945f55d37e0f6e1d` (+7 commits) |
| Reverie | unchanged, pin `dd3c178e`, hook-verified |
| Task state | `in_progress`, **`implemented` deliberately NOT set** |

### Denominator (step 1 of the task) — settled

Command, committed so it is re-runnable:

```
tests/backend-parity/classify_emission.py --population bucket
```

- `46/73` and `24/46/3` are **one** measurement (`24+46+3=73`), not two.
- **73 fixtures never existed** at any commit on any ref. Retire that number.
- `23/59` is right for a run *without* the portable profile; differs from mine
  by exactly one fixture (`cpuid_probe`).

Three legitimate denominators: **54/82** (directory), **56/85** (bucket),
**22/35** (buildable — the one describing real exposure).

### Progress on the three items

| Item | State |
|---|---|
| 1. De-blind fixtures | **16 of 22 done.** blind `22 → 6`; **accumulating-blind `12 → 0`**; `TALLY-ONLY 18 → 2` |
| 2. Plant agreeing-on-wrong-value | **Done**, bracketed both ways on 3 fixtures |
| 3. Check `ci=false` | **Done** — 1 of 85 tests has any mode enabled |

### Remaining 6 — three different kinds, needing three different decisions

1. **Mechanical de-alias, no judgement needed (2).** `getrusage_self_accounting`,
   `prctl_pdeathsig`. I had just read both when teardown was called; **not
   edited, nothing lost.** `prctl_pdeathsig` is a set/read-back round trip —
   emit `got` after each set (guest-determined). `getrusage_self_accounting`
   asserts determinized-to-zero rusage fields — emit the fields themselves; it
   is the same hidden-determinization shape as `statfs`/`uname`.
2. **Bare success STRINGS, need a real decision (3).** `numa_node_identity`,
   `prctl_identity`, `rlimit_identity` — they emit e.g. `rlimit-identity-ok`
   with no `key=value` at all. Needs a decision about what they should assert,
   not a reformat.
3. **Not an emission bug at all (1).** `cpuid_probe` already emits good values
   but **fails under the portable lane's `--no-virtualize-cpuid`** (host
   `AuthenticAMD` leaks through). Needs a **lane** decision.

### BLOCKER routed elsewhere — read before touching this family

**50 of 85 backend-parity-c fixtures do not compile** with the harness's own
command. Only 10 of 85 manifest blocks declare `cflags`. Verified by the harness
itself: `fixture preparation failed`. **Trap: do not add `-D_GNU_SOURCE`
globally** — `numa_node_identity` defines it itself and breaks under `-Werror`.

Routed to `hermit-parityc` (owns `backend-parity-c.toml`, branch
`ci/parityc-birth-default-and-enable-passing`). **I did not touch that manifest
(Invariant 2).** Also routed: **3 fixtures have no `[[test]]` entry at all** —
`sigaction_state`, `sigaltstack_state`, `sigprocmask_state`.

### Correct work order (emission fixes alone protect nothing)

make-it-build → register the 3 orphans → enable the cell → de-blind the rest.

### Correction to PR #1719's own commit message

It cites `rlimit_identity` as the model shape ("print the observed value"). It
is not — `rlimit_identity` prints `rlimit-identity-ok`, the blindest class.
Copy what #1719 *did* (`pipe_capacity`, `personality_domain`), not what it says.

---

## Reproducing the evidence

Env: `PKG_CONFIG_PATH` + `LIBRARY_PATH` → `ignored/lu-parity/usr/lib64`;
runtime `LD_LIBRARY_PATH=/home/newton/fbsource/fbcode/third-party-buck/platform010/build/libunwind/lib`.

Plant shims live in `ignored/plant/` (gitignored, will not survive teardown;
each is ~20 lines of `LD_PRELOAD` and is described in the commit messages).

No `validate.sh` receipt for either branch. All figures are **ptrace-only**.
