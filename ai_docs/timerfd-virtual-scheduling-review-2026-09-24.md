# Deterministic-scheduling review: virtual timerfd (separate from the socket fix)

Change class (skill §0): this adds a **virtual-time timer strategy for an
existing ready/wait strategy** — timerfd expiry, readiness, blocking, wake
ordering, relative/absolute deadlines, ties, timed waits, and fd lifecycle.
It changes no `scheduler.rs` code; it reuses the existing `SleepUntil` timed
waiter and timed-event machinery. Freeze: publication is this record plus the
timer-only diff (socket io-buffer and manifest changes are a separate change).

## Authority (§1)

- Capability source: `docs/DETERMINISM_ARGUMENT.md` rubric (scope claim,
  entropy inventory, mitigation) and the Hermit v2 roadmap timer/RCB notes.
- Standard scheduler: `detcore/src/scheduler.rs` (+ `timed_waiters.rs`,
  `real_timer.rs` read directly) — committed turns advance `committed_time`;
  timed events pop in `(deadline, tie-order)` sequence; when no thread is
  runnable the scheduler fast-forwards to the earliest timed event, which is
  the legacy order this change plugs into.
- Paper citation in the skill (Kaiser et al.) is an external link unreachable
  from this host (proxy 403); no claim here depends on it — the implemented
  scheduler code above is the normative reference used.
- Refusal boundary: under strict mode a blocking wait with no deterministic
  wake source must fail closed, never silently consume host timing. The
  disarmed-blocking-read path parks on `InternalIOPolling` (checked every
  scheduler visit) instead of inventing `EAGAIN`.
- Linux reference: `timerfd_create/settime/gettime` ABI (one-shot/interval,
  `TFD_TIMER_ABSTIME`, `TFD_TIMER_CANCEL_ON_SET` n/a on a never-jumping
  virtual clock, read returns `u64` expirations, wrong-type fd => `EINVAL`,
  small read buffer => `EINVAL`, level/edge semantics via epoll flags).
  Every behaviour below was first recorded natively on this host.

## Event path (§2)

`timerfd_create` (host fd created, state in open-file description) ->
`timerfd_settime` (validate, compute virtual deadline from
`thread_observe_time`; host fd never armed) -> producer wake sources are
virtual deadlines only -> waiter representation: `TimerfdVirtual{clockid,
deadline, interval, pending}` on the shared open-file description + epoll
interest recorded at `epoll_ctl` -> consumer checks: `read`, `poll`, `ppoll`,
`epoll_wait`, `epoll_pwait` each `collect(now)` (folds expirations at or
before virtual now; interval deadlines advance with `u128` math) and park via
`SleepUntil(min(deadline, timeout))`, a deterministic timed event in the same
queue as futex/sleep waiters. Host state cannot change the result: the only
host probe retained is for *non-timerfd* targets merged alongside.

Ties: two timerfds at one deadline both collect ready in fd order from the
interest map; a timerfd vs futex waiter at one deadline wakes in timed-event
queue order (existing scheduler tie rule, unchanged).

## Test trajectories (§3–§7, §9)

Battery `tfd_battery.c` (11 modes), native ground truth recorded first, then
identical Hermit output for every mode, strict `--verify` matched for
rel/periodic/tie/et/refuse plus the `timer-family-identity` fixture:

| Mode | Native | Hermit | Covers |
|---|---|---|---|
| rel | `poll=1 v=1` | same | relative one-shot |
| abs | `epoll=1 data=7 v=1` | same | `TFD_TIMER_ABSTIME`, epoll data |
| disarm | `poll=0 EAGAIN` | same | disarm before expiry |
| periodic | `total=3 interval=1ms remain>0` | same | interval counts, gettime |
| tie | `n=2 1 2` | same | two timers, same deadline |
| dup | `v=1` | same | dup shares description/state |
| fork | child consumes, parent `EAGAIN` | same | fork shared description |
| blockread | `v=1` | same | blocking read parked until armed by peer thread |
| refuse | pipe/nsec/flags/small all `EINVAL`, remain 4ms | same | refusal controls |
| et | `ET n1=1 n2=0`, `ONESHOT m1=1 m2=0` | same | edge/one-shot |
| exec | `poll=1 v=1` | same | non-CLOEXEC across exec |

Plus existing `detcore/tests/misc/notification_fds.rs` (timerfd, mixed epoll
sources — the merge case that found the tuple-write bug fixed in this diff):
4 passed. Unit: `TimerfdVirtual::collect` (boundary/interval/no-double-count).

Deterministic-review findings while testing: (1) synthesized epoll events
originally serialized fd+event tuples — guest data corruption, fixed and the
battery now compares full `data.u64`; (2) wrong-type `timerfd_settime`
returned `EBADF`, native is `EINVAL`, fixed. Neither involved weakening the
fixture or its comparator; `timer-family-identity` stays `ci:false`.

## Backends (§8) and evidence (§9)

- ptrace: full battery + fixture + notification tests (above), guests
  11/11 identical to native, verify matched on the strict runs quoted.
  Fixture qualification: 20 repetitions, 0 determinism divergences,
  17 matched + 3 host PMU `skid_overshoot` infrastructure errors — **not**
  clean 20/20, so the cell is not re-enabled by the socket change either.
- liteinst: same `DetcoreTool` handlers execute; local attempt is
  `backend-unavailable` (runtime DSO not built in this checkout), so liteinst
  is **not measured** here and no liteinst cell is re-enabled.
- sabre/kvm/dbt: timerfd cells are disabled for those backends in the
  manifests; this change does not touch their runners. Not claimed.

Residual, disclosed not fixed: `select/pselect6` and raw `epoll_pwait2`
(no typed Reverie variant) still consult the host for timerfd readiness;
`--no-sequentialize-threads`, namespace-only, record/replay modes are
outside this claim (record/replay replays the new syscalls as ordinary calls;
the host fd is created but never armed there either).

Verdict for the timer diff: correct under the frozen semantics above and
ready to publish **separately** from the socket fix, with
`post-facto-human-review` (determinization strategy). It does not land as
`ci:true` for any timer cell until a skid-free 20/20 exists.
