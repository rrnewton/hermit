# Current agent lifecycle review

Reviewed 2026-09-17 before consolidating the worker APIs. This is a source-level
review of the existing implementation, not an implementation plan or a claim
that `agentctl` exists. No runtime code was changed during this review.

Scope: `py/herdr_run/agent.py`, `subagents.py`, and
`foreign/{lib,agent_runner,agent_keeper}.py`, with their current tests. Chat
ownership and packaging dependencies are covered separately. Here, `subagents.py`
means the managed `herdr-agent` implementation; `foreign/` implements
`herdr-subagents`.

The shared interactive queue has substantial recovery and identity coverage.
The two worker managers nevertheless have different lifecycle guarantees, and
the headless runner does not inherit the shared queue's uncertain-submission
handling. Consolidation should preserve those distinctions until they are
explicitly resolved.

## Reproduced defects

These checks used temporary directories, deterministic fake clients, and one
disposable sleeping process standing in for a harness. They did not contact
Herdr, launch a real coding agent, or access the network. The sleeping process
was explicitly killed after the check; temporary directories were removed.
The reproduction snippets were not added as repository tests. Existing coverage
below was inspected; this audit did not rerun the complete test suite.

### 1. Concurrent sends with the same explicit message ID can submit twice

**Priority: high — duplicate agent work.**

[`agent.send`](../../../py/herdr_run/agent.py#L663) calls `_enqueue` with
`serialize=False`. Two callers can both pass the cross-directory existence check
at [line 292](../../../py/herdr_run/agent.py#L292). If the first caller creates
and drains its inbox file before the second reaches the atomic create, the
second can create the same ID in the now-empty inbox. The second drain submits
the prompt again, then discovers the collision when moving it to `processed`.

The fake-client interleaving produced **two identical submissions**, followed
by a refusal to overwrite `processed/same-id.json`; that ID remained in both
`processed` and `inflight`. Atomic creation in the inbox alone does not reserve
an ID across its entire lifecycle.

Existing coverage:
[`test_concurrent_explicit_message_id_is_created_exactly_once`](../../../py/tests/test_herdr_agent.py#L351)
uses the serialized `enqueue` entry point. It does not exercise this `send`
interleaving. Generated message IDs make this less likely for ordinary managed
sends, but the public library explicitly accepts caller-supplied IDs.

### 2. Stopping a busy headless worker can leave its harness running

**Priority: high — work continues after retirement.**

Both harness launch paths use `start_new_session=True`
([Codex](../../../py/herdr_run/foreign/agent_runner.py#L297),
[Antigravity](../../../py/herdr_run/foreign/agent_runner.py#L403)). The runner's
SIGTERM handler only sets a flag, checked outside the active turn.
[`bring_down_agent`](../../../py/herdr_run/foreign/lib.py#L2787) eventually calls
[`terminate_runner`](../../../py/herdr_run/foreign/lib.py#L1830), which signals
only the runner PID, not the separately launched harness process group.

A disposable harness read its prompt and slept while its runner waited on
stdout. `terminate_runner` returned success and the runner exited with SIGKILL;
the harness was still running. It was then explicitly killed by the check.
Retirement can archive state and remove the worker name while that work remains
active. Closing the presentation is not a reliable substitute for owning and
terminating the detached harness process tree.

Existing coverage:
[`test_packaged_runner_resumes_two_durable_turns`](../../../py/herdr_run/foreign/tests/test_agent_runner.py#L17)
terminates the runner after both turns finish. The
[forced-retirement test](../../../py/herdr_run/foreign/tests/test_backends.py#L1438)
has no live runner or active harness. Neither covers stop during a turn.

### 3. Migration activation failure leaves the registry pointing at the killed destination

**Priority: high — a live source becomes disconnected from its registry.**

[`migrate_agent`](../../../py/herdr_run/foreign/lib.py#L3412) saves the destination
backend, target, and PID before waiting for its activation acknowledgement. If
that acknowledgement times out, the exception path kills the destination and
clears the pause markers, but does not restore the source record
([rollback](../../../py/herdr_run/foreign/lib.py#L3423)). Its diagnostic then says
the failure occurred before commit and the registry remains unchanged.

With a fake activation timeout, the check observed a live source process,
a registry still pointing at the killed destination, and
`migration_failed_before_commit` claiming the registry was unchanged. Later
status or garbage collection can consequently reason about the wrong process.

Existing coverage:
[`test_migration_destination_runner_dies_keeps_tmx_source_intact`](../../../py/herdr_run/foreign/tests/test_backends.py#L1025)
fails before a staged identity exists. The
[success test](../../../py/herdr_run/foreign/tests/test_backends.py#L974)
always returns a successful activation acknowledgement. Neither covers failure
after the registry update.

### 4. A startup brief can be delivered to a replacement worker generation

**Priority: high — input reaches a different conversation than the launch requested.**

Managed [`start`](../../../py/herdr_run/subagents.py#L301) releases its lifecycle
lock before calling `self.send(name, brief)` and returning `self.status(name)`.
Another caller can stop the new worker and reuse its name in that gap. Both
subsequent operations resolve the name again, without checking the launch token.

The fake-client interleaving delivered the old startup brief to the replacement
pane and returned the replacement token from the original `start` call.
Keeping a durable generation token is insufficient unless the entire operation
remains bound to it.

Existing coverage: [initial briefs](../../../py/tests/test_herdr_subagents.py#L111)
and [sequential name reuse](../../../py/tests/test_herdr_subagents.py#L197) are
covered separately; this interleaving is not.

### 5. A partially allocated managed tab can become impossible to stop through the manager

**Priority: medium — recoverable launch failure strands managed state.**

[`_create_presentation`](../../../py/herdr_run/subagents.py#L323) saves the new
`tab_id` before discovering its `pane_id`. A transient pane-list failure leaves
an inspectable `launch_failed` record with a live tab but `pane_id=None`. Once
Herdr becomes available, [`stop`](../../../py/herdr_run/subagents.py#L512) rejects
the tab's actual pane as changed ownership. The documented normal cleanup path
cannot release the name.

The fake-client check reproduced this with one failed discovery request. The
existing [failed-launch cleanup test](../../../py/tests/test_herdr_subagents.py#L135)
fails later, during harness startup, after both identities were recorded.

### 6. Managed stop can close a human pane added after its ownership check

**Priority: medium — the documented teardown refusal has a race.**

[`stop`](../../../py/herdr_run/subagents.py#L507) checks a snapshot of tab
membership, then captures output, writes state, and closes the whole tab at
[line 527](../../../py/herdr_run/subagents.py#L527). The manager's file lock does
not serialize human changes made through Herdr.

Adding a second pane during the fake output read resulted in successful closure
of both panes. The [existing extra-pane test](../../../py/tests/test_herdr_subagents.py#L171)
checks only a pane present before stop starts. This matters to any future
promise of direct human interaction alongside automated lifecycle control.

### 7. A headless turn can be replayed after a crash between execution and consumption

**Priority: medium now; high before adding automatic restart.**

[`_consume`](../../../py/herdr_run/foreign/agent_runner.py#L493) executes the
entire turn while its request remains in `inbox`, then moves the request to
`processed`. There is no inflight barrier or reconciliation against a completed
turn record. A crash after execution, including after writing a completion
sentinel, leaves the request looking pending.

The isolated check injected a failure into that final move, then consumed the
same pending file again. The fake turn ran twice with the same sequence number.
This demonstrates the recovery hazard; it does **not** claim an automatic
runner restart already exists. A restarted runner using that inbox, or a future
supervisor added during consolidation, would repeat the request without first
classifying its prior execution as uncertain.

Existing coverage:
[two normal headless turns](../../../py/herdr_run/foreign/tests/test_agent_runner.py#L17)
are tested. The shared interactive queue has explicit crash-barrier tests, but
headless `_consume` does not use that implementation.

### 8. A failed Codex turn can report the previous turn's final answer

**Priority: medium — output is correlated with the wrong request.**

Every Codex turn uses the same
[`last-message.txt` output path](../../../py/herdr_run/foreign/agent_runner.py#L160).
The runner does not clear or version the file before launch. If the next turn
fails before replacing it, [completion handling](../../../py/herdr_run/foreign/agent_runner.py#L367)
reads the old answer and appends it beneath the new turn's `OUTPUT` marker. A
nonempty old answer also suppresses the synthesized current harness error.

With a preexisting answer and a fake process that emitted an error and exited
nonzero without writing an answer file, the new failed turn contained the prior
answer and `read --last` still returned that prior answer. The exit status
remained nonzero; the defect is stale answer association, not a fabricated
successful exit code.

Existing coverage: the fake Codex in the
[two-turn runner test](../../../py/herdr_run/foreign/tests/test_agent_runner.py#L28)
always replaces the answer file. It does not model failure before that write.

## Source-only risks and boundaries requiring explicit contracts

### Rust counterparts

A bounded source check found the same four managed/shared control-flow patterns
in the Rust implementation. These are **source-level matches, not additional
Rust reproductions**. Fixes and regression coverage need to address both
implementations.

| Python finding | Matching Rust control flow |
| --- | --- |
| Explicit-ID send replay | [`agent.rs`](../../../rs/herdr-run/src/agent.rs#L693) calls `enqueue_internal` without serialization for identified sends. Its cross-directory existence check precedes the inbox-only atomic create. |
| Startup brief crosses generations | [`subagents.rs`](../../../rs/herdr-run/src/subagents.rs#L573) drops the lifecycle lock before sending the brief and reading status by name, without checking the launch token again. |
| Partial tab cleanup failure | [`subagents.rs`](../../../rs/herdr-run/src/subagents.rs#L631) persists the tab before pane discovery; stop rejects the surviving pane when the recorded pane is absent. |
| Stop uses stale tab membership | [`subagents.rs`](../../../rs/herdr-run/src/subagents.rs#L1000) snapshots panes, then reads output and writes state before closing the whole tab. Human membership changes are not serialized with that operation. |

### Other source-only observations

These were identified from control flow and current test coverage; they were
not independently reproduced during this bounded audit.

| Area | Concrete source observation and consequence | Coverage or preservation implication |
| --- | --- | --- |
| Foreign teardown ownership | [`kill_window`](../../../py/herdr_run/foreign/lib.py#L1772) closes the recorded Herdr tab without checking its current panes or harness identity. tmux records use mutable session/window names, and exact-name matching does not establish that the window still belongs to this worker. Normal `down` and headless GC both call this path. | The [teardown test](../../../py/herdr_run/foreign/tests/test_backends.py#L671) establishes only that the workspace is not closed. Do not transfer the managed manager's stronger ownership claims to this runtime. |
| Foreign lifecycle transactions | [`enqueue_message`](../../../py/herdr_run/foreign/lib.py#L1070) allocates a sequence under the registry lock but publishes its file after releasing it. [`bring_down_agent`](../../../py/herdr_run/foreign/lib.py#L2782) reads, waits, archives, and eventually removes the name across separate critical sections. A sender can publish into a recreated state directory after archival, or a stale operation can affect a reused name. Startup likewise updates/removes records by name after an unlocked launch. | Unique sequence allocation is not a full lifecycle transaction. The inspected foreign tests do not exercise send-versus-stop or start-versus-name-reuse interleavings. Generation identity and request publication need a shared contract. |
| A migrated runner's next migration | The runner reads `stage_token` once, acknowledges activation, then retains that value. Its ordinary intake loop checks migration-pause markers only when `stage_token is None` ([runner](../../../py/herdr_run/foreign/agent_runner.py#L505)). A previously staged runner therefore does not acknowledge a later pause. | A later migration from Herdr can time out; the tmux path falls back to its older stop/restart procedure. Current migration tests replace the handshake with fakes and do not run two successive migrations through a real runner. |
| Codex pipe draining | The runner iterates stdout until EOF, then waits for exit, then reads stderr ([runner](../../../py/herdr_run/foreign/agent_runner.py#L334)). A child that fills stderr before closing stdout blocks on the pipe and can be killed by the turn timeout. | The fake Codex tests do not fill stderr. Timeout classification alone does not establish that the harness itself was hung. |
| Native conversation identity | A fresh foreign TUI record starts with `session_id=None`; [`TuiProbe`](../../../py/herdr_run/foreign/lib.py#L1587) collects process/readiness data but not a native conversation ID. Its shared delivery target therefore lacks a session assertion unless a prior headless session supplied one. | A pane/harness match does not prove continuity after `/new`, `/resume`, or a same-harness replacement. Managed binding is stronger when native metadata is available, but unsupported identity must remain explicitly unverified. |
| Disk durability | Foreign registry, sidecar, inbox, and archive changes use file writes and rename without the shared queue's file/directory `fsync` protocol ([registry](../../../py/herdr_run/foreign/lib.py#L762), [inbox](../../../py/herdr_run/foreign/lib.py#L1081)). | Atomic visibility and process-crash behavior should not be described as the same power-loss durability contract as the shared queue. The latter has dedicated fsync coverage. |
| Keeper lifecycle | `agent_keeper` is an explicit helper that selects the keeper by label and runs periodic status commands. No calls from the inspected `up`/`down` library paths automatically enforce its population-transition description. | It is not a supervisor, durable worker identity, or proof that a terminal is safe to close. Keep presentation convenience separate from worker correctness. |

## Existing strengths to preserve

- **Pending and uncertain are different states.** A busy wait retains a request
  without consuming a retry attempt. The shared queue moves a request behind
  its durable inflight barrier before terminal submission; a process restart
  quarantines it instead of automatically injecting it again. Tests cover
  [busy restart](../../../py/tests/test_herdr_agent.py#L667),
  [crash after submission](../../../py/tests/test_herdr_agent.py#L621),
  [crash immediately after the barrier](../../../py/tests/test_herdr_agent.py#L641),
  and [distinct machine outcomes](../../../py/tests/test_herdr_agent.py#L694).
- **Different queue roots share a target lock.** Exact-pane and session-based
  addressing serialize on the resolved pane, and target assertions are checked
  again while waiting. See [shared-target tests](../../../py/tests/test_herdr_agent.py#L206).
- **Known identity changes are refusals.** The managed API validates its name,
  pane, workspace, directory, and known native session. Tests cover
  [session replacement](../../../py/tests/test_herdr_subagents.py#L171) and
  [named-agent replacement](../../../py/tests/test_herdr_subagents.py#L291).
- **Ordinary managed mutations use a stable lifecycle lock.** Send, drain,
  read, and stop share the lock outside the worker directory, so archiving and
  recreating that directory cannot replace a held lock inode. The startup-brief
  gap and human terminal changes remain the exceptions identified above.
- **Unavailability is not automatically death.** Managed status retains its
  registry on a failed probe. Foreign GC preserves live runners whose views
  disappeared and preserves state when Herdr workspace/TUI probes fail.
  [Workspace-loss tests](../../../py/herdr_run/foreign/tests/test_backends.py#L690)
  distinguish a failed probe from confirmed loss and preserve a recovery snapshot.
- **Headless view repair need not restart a conversation.**
  [`recreate_window`](../../../py/herdr_run/foreign/lib.py#L3476) requires a live
  runner and repairs its presentation or creates a transcript view. It refuses
  to guess a replacement interactive TUI. A dead runner and a lost view remain
  different cases.
- **Migration has useful existing safeguards.** Idle/empty-inbox checks,
  source-pause acknowledgement, staged destination identity, and an explicit
  resumed-TUI health check are already implemented. Preserve these checks while
  correcting the post-commit failure behavior; a CLI rename must not bypass them.
- **Permissions and output semantics are explicit.** Harness permission choices
  survive older registry writers through sidecar metadata. TUI reads refuse
  durable last-answer/turn-boundary modes rather than silently substituting
  terminal scrollback. Native goal reads remain distinct from requested goal
  text and prompt-readiness observations.

The extraction boundary should retain these established behaviors and tests,
while carrying the reproduced defects as separate correctness work. It should
not introduce automatic restarts, stronger identity claims, or seamless human
handoff based solely on renaming the existing commands.
