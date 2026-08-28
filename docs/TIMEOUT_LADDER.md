# The timeout ladder

Several independent mechanisms can stop a Hermit run. They are nested, they do
**not** bound the same thing, and they report differently. This file says what
each one bounds, how to tell which one fired, and what has to stay true between
them.

**If a run was killed rather than failing, this is the page for it.** The same
number means different things at different rungs, so two rungs carrying "15" are
not agreeing with each other. Five rungs exit **124**, which is why the exit code
cannot say which one fired and the stderr class line is the discriminator
instead. And a bound at an inner rung that is not strictly smaller than its outer
rung can never fire: it is dead configuration that reads as protection, and it
presents as an intermittently killed test rather than as the configuration error
it is.

`ci/validate-timeout-layers-test.sh` proves a named dagrun step timeout and the
outer scope timeout with live sleepers, and audits the direct portable strict
probe bounds. This is the same idea widened to every rung, including the ones
that script does not cover.

All values measured 2026-08-26 against `rrnewton/hermit` `main`
`f77d7c44067a12ba11e75b5a85864ce0bc23e8f4`. Re-measure before quoting them; the
point of this file is the *structure*, and the numbers move.

## What each rung bounds

⚠️ **THE UNITS DIFFER, AND THAT IS THE WHOLE TRAP.** "Fifteen seconds" means a
different quantity at each rung, so two rungs carrying the same number are not
agreeing with each other.

| Rung | Bounds | Value comes from | How it stops the run | Reports |
| --- | --- | --- | --- | --- |
| `hermit run --timeout N` | **one hermit invocation** = one guest execution, **ptrace and liteinst only** | the caller's argument | hermit drops the guest future and unwinds its own container | exit 124, `HERMIT_RUN_TIMEOUT class=run-timeout` |
| hermit's unwind fallback | the same invocation, `N + 10s` | `RUN_TIMEOUT_UNWIND_GRACE` in `hermit-cli/src/lib.rs` | `_exit(124)` from a `SIGALRM` handler; no destructors | exit 124, `HERMIT_RUN_TIMEOUT_FALLBACK` |
| `hermit record --record-timeout N` | one recording | the caller's argument | `_exit(124)` from a `SIGALRM` handler | exit 124 |
| nextest `slow-timeout` | **one cargo test process**, which may invoke hermit zero or many times | `.config/nextest.toml`: 15s default, two named 30s overrides | `SIGTERM` to the test binary, 2s grace, then `SIGKILL` | wrapper exit 100, test named by nextest |
| manifest cell `timeout_seconds` | **one manifest cell** | `tests/e2e/manifests/*.yaml`, per cell | `timeout --kill-after=10s Ns` around the cell command | exit 124 |
| dagrun step `timeout` | **one DAG node**, i.e. a whole batch of cells or tests | `ci/dag/{portable,privileged}.json` | dagrun stops the step | node failure |
| portable strict compatibility node | one direct `compat.*` probe in the outer DAG | `ci/dag/portable.json` | dagrun stops the named probe | 60s normally; 20s for the three declared portable diagnostics |
| `safehermit --sh-deadline` | **the whole wrapped process tree** | `bin/safehermit`, default 3600s | `systemd-run --user RuntimeMaxSec`, a **cgroup kill** | exit 124, `safehermit: bound.wall=` |

Distribution of the values actually deployed today:

- manifest `timeout_seconds`: 90s ×310, 60s ×25, 120s ×22, 600s ×1.
- dagrun step `timeout`: 60s ×193, 600s ×15, 900s ×15, 120s ×12, 180s ×6,
  720s ×4, 1200s ×4, 300s ×3, 20s ×3, 2400s ×1, 420s ×1, 40s ×1,
  30s ×1.

## Gentle first, hard as fallback

Owner ruling: a run is stopped by a **proper teardown with a gentle kill of
hermit itself**, and a hard kill is the fallback, never the first move. Hermit
made the container, so hermit unwinds it.

Only the innermost rung can do that, because it is the only one running inside
the process that owns the container and the log. The rungs above it stop hermit
from outside; the best they can do is stop it *cleanly*, which is not the same
as unwinding.

⚠️ **A CGROUP KILL CANNOT BE GENTLE, BY CONSTRUCTION.** `safehermit`'s deadline
is `SIGKILL` delivered to every cgroup member at once, with no `SIGTERM` phase
and no opportunity for any member to run a destructor. This is not a defect to
repair — it is what that rung is *for*: the runaway that does not answer. It
stays as the outermost backstop and should not be pressed into service as a
per-cell bound.

⚠️ **AN EXTERNAL KILL DOES NOT LEAVE PROCESS RESIDUE TODAY, AND THE ARGUMENT
THAT IT DOES IS OUT OF DATE.** Measured 2026-08-26: `SIGTERM` to the outer
`hermit` process left no surviving guest and no surviving hermit — the outer
exited 143 and the run was gone. `PR_SET_PDEATHSIG` and the container-init stop
handlers in `hermit-cli/src/bin/hermit/container.rs` already closed that hole,
which is why three runs once survived 45 hours and none does now. The case for
the innermost rung is **not** that the outside cannot clean up. It is that only
the inside can produce a *record* of what happened before it stops: an
externally killed run writes no verdict and a truncated log. The residue is
evidence, not processes.

## Which rung fired: read the class line, not `$?`

⚠️ **THE EXIT CODE CANNOT ANSWER THIS AND MUST NOT BE ASKED.** Five different
rungs exit **124** — hermit's own bound, hermit's fallback, `hermit record`'s
bound, the manifest cell's `timeout(1)`, and `safehermit`'s wall deadline. GNU
`timeout` uses 124 for the same event. A consumer that branches on `$?` alone is
guessing which mechanism stopped the run, and will attribute a slow guest to the
wrong layer.

**The contract is the class line on stderr.** Each rung emits a distinct,
greppable marker, and that marker — not the status — is what a caller keys on:

| Marker | Meaning | What to change |
| --- | --- | --- |
| `HERMIT_RUN_TIMEOUT class=run-timeout` | hermit's own bound expired and hermit unwound the container. The line also carries the bound in seconds. | the guest is genuinely slow, or the bound is too tight |
| `HERMIT_RUN_TIMEOUT_FALLBACK` | the bound expired and **the unwind itself did not finish** within the grace. This is a hermit defect, not a slow guest. | investigate the wedged teardown |
| `safehermit: bound.wall=APPLIED` … then a kill | the outermost cgroup deadline reaped the tree | the run escaped every inner bound |
| a nextest-named test with wrapper exit 100 | the test **process** exceeded its per-test cap | `.config/nextest.toml` |
| exit 124 with **no marker at all** | something outside hermit killed it — `timeout(1)` on the cell, or a harness | the cell's `timeout_seconds`, or a missing inner bound |

That last row is the useful one. **A cell that times out with no class line means
no inner bound fired**, which is either a missing `--timeout` or an inner bound
set larger than the outer one. Both are configuration errors, and the marker's
absence is what distinguishes them from a genuinely slow guest.

## `--timeout` is qualified per backend, and refuses elsewhere

⚠️ **A TIMEOUT FLAG READS AS BACKEND-AGNOSTIC AND THIS ONE IS NOT.** Measured
2026-08-26 with `--timeout 3` against a guest that never exits, two runs each:

| Backend | Elapsed | Marker | Meaning |
| --- | --- | --- | --- |
| `ptrace` | 3s | `class=run-timeout` | bound works, container unwound |
| `liteinst` | 3s | `class=run-timeout` | bound works, container unwound |
| `kvm` | 13s | `HERMIT_RUN_TIMEOUT_FALLBACK` | bound holds, but ONLY via the hard fallback |
| `sabre` | 40s | none | **did not bound the run at all** |
| `dbt` | 20s | none | **did not bound the run at all** |

⚠️ **READ THAT TABLE AS A DEFECT REPORT, NOT A SUPPORT MATRIX.** `sabre` and
`dbt` are not backends that merely lack testing — **they structurally cannot
honour the flag today.** The 40s and 20s are the *harness's* own deadline, not
hermit's, and the missing marker is exactly the "exit 124 with no marker"
reading above: the bound was accepted and then had no effect whatsoever. All
five backends run correctly **without** the flag, so it is the bound that fails,
not the backend.

The mechanism is visible on `sabre`, which additionally panicked in reverie's
blocking RPC transport after **69 seconds** — `reverie-rpc-transport`'s
`blocking_client`, reporting a broken pipe once the container went away. A
*blocking* call cannot yield to the single `current_thread` tokio runtime that
`tokio::time::timeout` needs in order to fire, so the primary path is never
reached. `dbt` shows the same absence of any bound and is a launch adapter
around DynamoRIO, so it is very likely the same class of problem; that has not
been traced and should not be asserted.

⚠️ **THE DISTINCTION MATTERS FOR WHOEVER FIXES IT.** "Unverified" invites
someone to run the flag once, see it accepted, and mark the backend supported.
What is actually required is finding out why the runtime never reaches the timer
and fixing that — and until it is fixed, the refusal below is the honest
behaviour rather than a placeholder.

So `--timeout` **refuses** (exit 122, `HERMIT_POLICY_REFUSAL`) on any backend
other than `ptrace` and `liteinst`, rather than accepting a bound it cannot
enforce. `kvm` is refused for a softer reason than the other two: it does bound
the run, but every KVM timeout would take the hard `_exit` path and emit the
marker that is supposed to mean *the unwind failed*, destroying the signal that
marker carries. Qualify a backend by finding out why the runtime never reaches
the timer — not by widening the list.

Fail-closed, so a new backend must be qualified deliberately instead of
inheriting a guarantee nobody measured for it.

## The invariant

**Each rung must be strictly smaller than the rung outside it.**

If an inner bound is greater than or equal to its outer bound, the inner one can
never fire. It is then dead configuration that reads as protection — and the
run is stopped by the outer rung instead, hard and unnamed. The symptom is a
test that looks intermittently killed for no stated reason, which is why this
belongs in a configuration check and not in a debugging session.

`ci/validate-timeout-layers-test.sh` enforces the direct compatibility-probe
bounds and separately exercises the dagrun-step and outer-scope rungs. Nothing
enforces the invariant across the manifest, nextest and native rungs yet.

## Reading a "global default" for the manifest

⚠️ **EVERY CELL ALREADY DECLARES AN EXPLICIT `timeout_seconds`.** Measured
2026-08-26: **358 of 358 cells across all 13 manifests**, with no gaps. That
makes "global default" ambiguous in a way that has opposite consequences, so the
schema has to say which it means:

- **As a fallback for cells that omit the field**, it applies to **0 of 358**
  cells on the day it lands. It is correct, and it is also inert until some
  future cell omits the field — the shape of mechanism this project has been
  bitten by before.
- **As a value that overrides the per-cell declaration**, it retimes **310
  cells from 90s to 15s at once**, a six-fold tightening. Already known to
  bite: `applications/timed-progress-bar` declares 120s and measures about 18s,
  so it fails immediately.

Neither is wrong; they are different features. The per-cell override to 30s that
the owner named is a third, separate thing and does not resolve the ambiguity.

## Known gap

`hermit-cli/tests/container_init_deadline.rs` — which defends `PR_SET_PDEATHSIG`
and the container-init stop handlers, i.e. the guarantee that an external
deadline can end a hung run at all — **is in no DAG node**. Enumerating every
`--test <target>` across `ci/dag/portable.json` and `ci/dag/privileged.json`
yields 50 targets and that file is not among them, so those cells never run in
validation. The regression cells for `hermit run --timeout` are in
`hermit-cli/tests/cli.rs` for that reason.

If that file is ever wired in, note that it declares a 15-second deadline, a
12-second startup budget and a 20-second teardown budget, so it needs a named
per-test override above the 15-second nextest default or it will cut its own
tests and present as a product bug.
