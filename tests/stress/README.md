# Concurrency stress suite

`concurrency.rs` is a parameterized guest for Hermit chaos testing. It covers:

- lost atomic updates;
- incorrect publication ordering;
- producer/consumer completion races;
- missing barrier synchronization;
- lost condition-variable wakeups;
- mutex correctness under contention;
- bounded `RwLock` writer fairness; and
- the Relaxed-atomic store-buffer litmus.

The guest exits with status 1 when it observes the target race and status 0 when
the invariant holds. The store-buffer category documents a current limitation:
Hermit serializes guest threads and therefore does not explore weak-memory
outcomes that can occur during native parallel execution.

The integration harness is `hermit-cli/tests/stress_suite.rs`. Its fast tier
runs ten fixed chaos seeds with the random scheduling heuristic at 2, 4, 8, and
16 threads. The slow tier runs 100 seeds for the sparse 16-thread
producer/consumer and condition-variable races. A separate PMU-dependent tier
searches the existing CAS handoff race with imprecise timers, records a failing
preemption schedule, and requires a precise replay of that failure.

Run the tiers with:

```bash
cargo test -p hermit --test stress_suite fast_chaos_matrix -- --exact
cargo test -p hermit --test stress_suite slow_race_matrix -- --exact
cargo test -p hermit --test stress_suite slow_cas_search_and_replay -- --exact
```
