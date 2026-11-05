# Public agent lifecycle review

Read-only source review, 2026-09-17. No agents were launched and no upstream tests
were executed. This is a bounded comparison of implementation contracts, not a
reliability ranking or endorsement. A feature name or README claim is not evidence
that the implementation confirms its advertised outcome.

Sources are pinned to Happy `3fd0be9e2afb19cce67fed40af379db5e73b7d27` and Claude
Squad `ce1ffb4392b01f38e2c4599c7c84d2a93973b138`. Happy's agent client, daemon,
Claude/Codex wrappers, session API, and relevant server handlers were inspected.
For Claude Squad, the app, instance/storage, and tmux implementations were
inspected. Other entry points and deployment behavior may differ.

## Happy: useful mechanisms and materially different command guarantees

### 1. `happy-agent send` does not confirm durable receipt

**Observed:** [`SessionClient.sendMessage`][happy-send] emits a Socket.IO message
containing session ID and encrypted content. It supplies neither a `localId` nor
an acknowledgment callback. The non-waiting [`send` action][happy-send-action]
waits 500 ms to let the event flush, closes the connection, and prints
`sent: true`. That establishes an attempted send, not server persistence or
harness acceptance. A caller retry has no stable request identity in this path.

This is narrower than saying Happy lacks message deduplication. Its server
[WebSocket handler][happy-server-message] checks `localId` when supplied. The
[HTTP v3 handler][happy-v3] deduplicates supplied IDs, and the database has a
[unique `(sessionId, localId)` constraint][happy-message-schema]. The wrapper's
[outbox][happy-wrapper-outbox] generates IDs and posts through v3. Those stronger
paths do not automatically strengthen the separate `happy-agent send` path.

**Design consequence:** expose distinct outcomes for attempted, durably accepted,
harness accepted, and completed. Use one request identity from caller through
reply, including retries after a lost acknowledgment.

### 2. `send --wait` can observe another turn; `wait` is not a busy-state proof

**Observed:** the CLI installs [`waitForTurnCompletion`][happy-wait-turn] before
emitting the message. This avoids missing a fast response, but the watcher is
session-wide and is not linked to the submitted request. It accepts a `turn-end`
when no active turn ID has yet been observed, or a matching/null turn ID later.
It also has legacy ready-event and idle-state fallbacks.

**Inferred failure case from the condition:** if a session was already working
when the watcher attached, that earlier turn's completion can satisfy the wait
before the new request starts. This was not reproduced live.

The separate [`waitForIdle`][happy-idle] uses an agent-state predicate:
`controlledByUser` is false and `requests` is empty. It does not test the
wrapper's `thinking` heartbeat or queued work. An initialized state satisfying
that predicate resolves immediately. In contrast, the Codex wrapper's
[`emitReadyIfIdle`][happy-codex-ready] checks pending input, queue size, and exit
intent before emitting ready. These are different definitions of idle.

**Design consequence:** waiting for a request must identify its turn/result.
Keep process alive, connected, accepting input, busy, waiting for approval, and
completed as different facts. A heartbeat or empty permission queue is not a
turn-completion event.

### 3. `happy-agent stop` sends a presence notification, not a kill request

**Observed:** [`stop`][happy-stop-action] calls `sendStop`, waits 500 ms, and closes
the socket. [`sendStop`][happy-stop] emits `session-end`. The
[server handler][happy-server-stop] clears presence updates, sets `active: false`,
and broadcasts an ephemeral update to user-scoped clients. It does not invoke a
daemon stop or send a termination RPC to the wrapper. The wrapper itself calls
the same event [`sendSessionDeath`][happy-death-event].

Happy has actual termination paths elsewhere: the Codex wrapper's
[`killSession` handler][happy-codex-kill] aborts work, closes the backend, and
exits; the daemon's [`stopSession`][happy-daemon-stop] sends signals. They are
not called by the reviewed `happy-agent stop` action. Consequently that action
does not establish that execution stopped; a running wrapper can continue to
send its presence heartbeat.

**Design consequence:** disconnecting a client, publishing inactive presence,
interrupting a turn, requesting process termination, and observing process exit
must have distinct API results. Report stop success only at the promised level.

### 4. Spawn confirmation is stronger than a PID, but weaker than harness readiness

**Observed:** the daemon's [detached spawn path][happy-spawn] stores the child,
listens for early exit/error, and waits up to 15 seconds for a local registration
webhook carrying a Happy session ID. The [webhook handler][happy-webhook] checks
a reserved reconnect identity before persisting it. Spawn success therefore
confirms registration, rather than merely successful `spawn()`.

The timeout callback returns an error and removes waiters without killing the
child. A timeout can leave a live process that reports in later. The requesting
[`happy-agent` machine RPC][happy-spawn-rpc] has a separate 30-second timeout and
no caller-provided idempotency key. Neither timeout proves the spawn did not
happen. The Codex wrapper [registers with the daemon][happy-codex-register]
before it [connects the Codex backend][happy-codex-connect]; registration does not
prove the harness has become ready to accept work.

**Design consequence:** preserve an uncertain or still-starting operation with
a stable identifier. Reconcile it before creating a replacement. Define
registration and harness readiness separately.

### 5. Resume and stop contain valuable ownership checks

**Observed:** the daemon [loads persisted session identities][happy-persist-load]
on restart. [`resumeSession`][happy-resume] coalesces concurrent attempts in one
daemon and refuses sessions still registering or stopping. A saved live PID
from a previous daemon is treated as a possible conflict, not authority to adopt
or kill it. The [liveness helper][happy-liveness] treats only `ESRCH` as proof of
absence and accounts for machine boot time.

The daemon's [actual stop path][happy-daemon-stop] targets the process group for
its detached children, falls back to the parent where necessary, and retains
ownership until an exit event or liveness check observes death. Its boolean
return can still mean only that termination was requested. These are useful
mechanisms to study independently of the weaker `happy-agent stop` entry point.

**Bounded conclusion:** this establishes recovery and duplicate-owner checks,
not automatic restart of all crashed workers. The reviewed child-exit path
preserves metadata for an explicit resume; it does not itself respawn the child.

### 6. Network reconnect and process-crash recovery are different

**Observed:** the wrapper [checks message sequence continuity][happy-receive]
and fetches missing server history when the socket reconnects or a sequence gap
appears. However, its receive cursor, inbound pending messages, and outbox are
in-memory fields. The outbox is retained until its HTTP call returns; an abrupt
process death can lose records that have not been persisted remotely.

For a resumed Codex process, the wrapper explicitly [skips existing messages on
reconnect][happy-codex-register]. The receive implementation then advances its
cursor without routing those old messages. That avoids indiscriminate replay,
but is not a per-request reconciliation of “server received, harness never
started” versus “harness already performed effects.”

**Design consequence:** crash recovery needs a durable delivery ledger at the
harness boundary, not just a durable chat history. Unsupported certainty should
be reported as uncertain rather than converted into automatic replay or success.

### 7. Human/remote handoff differs between Claude and Codex

**Observed:** Claude's [local launcher][happy-claude-local] responds to a remote
queued message by aborting the local child and requesting a switch. The
[outer loop][happy-claude-loop] runs the local and remote launchers sequentially,
passing the session's native conversation ID. The
[remote switch path][happy-claude-remote] aborts and awaits its current operation
before switching back. This is a handoff between executions, not two independent
controllers typing into the same always-live native TUI.

The reviewed Codex path [renders Happy's own Ink display][happy-codex-display]
and drives a Codex app-server client. It should not be described as the same
native-Codex-TUI handoff mechanism simply because the root README discusses
local and remote use together.

**Design consequence:** describe each harness's actual human interface and
ownership transition. Session continuity, process continuity, and native TUI
attachment are separate capabilities.

## Claude Squad: visible terminals do not establish lifecycle correctness

### 8. Restore checks terminal existence, not native harness identity

**Observed:** [tmux startup][squad-start] waits up to two seconds for session
existence. The [existence check][squad-exists] correctly uses exact matching.
Names are derived by removing whitespace and replacing dots in the display
title, rather than by storing a native conversation identity.

On dashboard restart, [stored instances][squad-storage] restore their tmux
connections. If a tmux session disappeared, [restoration][squad-restore] marks
the instance paused while retaining its workspace. [Resume][squad-resume]
preserves a valid surviving worktree and either reattaches or starts a new
program. No native conversation ID is present in the reviewed stored schema;
restarting the program does not by itself prove it resumes the previous agent
conversation.

### 9. Prompt submission and readiness are terminal heuristics

**Observed:** [`SendPrompt`][squad-send] writes prompt bytes to the PTY, sleeps
100 ms, and writes Enter. It has no request ID or harness acceptance/completion
receipt. The [status monitor][squad-status] hashes captured pane text and
recognizes selected permission-prompt strings. A capture error returns
`false, false`; the [app maps unchanged/no-prompt output to Ready][squad-ready].
Thus quiet output, and even a failed capture, can be reported as ready without
an explicit harness signal. These paths do not provide durable messaging or
duplicate/uncertain-delivery handling.

### 10. Detach, pause, and deletion have materially different effects

**Observed:** [dashboard quit][squad-quit] saves instances and exits. The tmux
[detach operation][squad-detach] closes the client PTY; [Close][squad-close]
invokes `tmux kill-session`. These are distinct operations.

More surprisingly, [`Pause`][squad-pause] calls `DetachSafely`, may commit dirty
work, and removes the worktree while preserving the branch. It does not call
`Close`; Resume explicitly handles a tmux session that still exists from pause.
The pause comment says it stops the tmux session, but the implementation does
not establish that the agent process has stopped before removing its working
directory. [`Kill`][squad-kill] attempts workspace cleanup even if terminal
termination failed. Those are important coupling risks for a supervisor.

**Design consequence:** worker lifecycle and worktree lifecycle need an explicit
ordering contract. A name such as pause must state whether work is interrupted,
the process is stopped, the viewer is detached, or the workspace is removed.

## What this review supports

Use these implementations as sources of concrete mechanisms and failure cases,
not as a quality bar. In particular, evaluate a proposed supervisor against
observable contracts: stable operation identity, confirmed registration/readiness,
durable request/result correlation, conservative reconciliation after lost
acknowledgments, verified termination, and honest native-interface capabilities.
No source review here establishes that either project, or the proposed utility,
satisfies that entire contract.

[happy-send]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-agent/src/session.ts#L192-L209
[happy-send-action]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-agent/src/index.ts#L380-L413
[happy-server-message]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-server/sources/app/api/socket/sessionUpdateHandler.ts#L186-L245
[happy-v3]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-server/sources/app/api/routes/v3SessionRoutes.ts#L148-L211
[happy-message-schema]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-server/prisma/schema.prisma#L148-L162
[happy-wrapper-outbox]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/api/apiSession.ts#L663-L701
[happy-wait-turn]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-agent/src/session.ts#L289-L364
[happy-idle]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-agent/src/session.ts#L22-L42
[happy-codex-ready]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/codex/emitReadyIfIdle.ts#L13-L26
[happy-stop-action]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-agent/src/index.ts#L446-L469
[happy-stop]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-agent/src/session.ts#L367-L376
[happy-server-stop]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-server/sources/app/api/socket/sessionUpdateHandler.ts#L248-L291
[happy-death-event]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/api/apiSession.ts#L872-L889
[happy-codex-kill]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/codex/runCodex.ts#L474-L517
[happy-daemon-stop]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/daemon/run.ts#L872-L956
[happy-spawn]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/daemon/run.ts#L614-L699
[happy-webhook]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/daemon/run.ts#L213-L274
[happy-spawn-rpc]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-agent/src/machineRpc.ts#L59-L135
[happy-codex-register]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/codex/runCodex.ts#L230-L255
[happy-codex-connect]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/codex/runCodex.ts#L826-L841
[happy-persist-load]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/daemon/run.ts#L175-L207
[happy-resume]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/daemon/run.ts#L726-L761
[happy-liveness]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/daemon/sessionLiveness.ts#L1-L23
[happy-receive]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/api/apiSession.ts#L289-L345
[happy-claude-local]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/claude/claudeLocalLauncher.ts#L75-L125
[happy-claude-loop]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/claude/loop.ts#L81-L117
[happy-claude-remote]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/claude/claudeRemoteLauncher.ts#L73-L101
[happy-codex-display]: https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-cli/src/codex/runCodex.ts#L519-L556
[squad-start]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/session/tmux/tmux.go#L68-L156
[squad-exists]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/session/tmux/tmux.go#L469-L472
[squad-storage]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/session/storage.go#L10-L35
[squad-restore]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/session/instance.go#L248-L263
[squad-resume]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/session/instance.go#L509-L571
[squad-send]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/session/instance.go#L631-L649
[squad-status]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/session/tmux/tmux.go#L239-L266
[squad-ready]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/app/app.go#L238-L260
[squad-quit]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/app/app.go#L346-L350
[squad-detach]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/session/tmux/tmux.go#L346-L384
[squad-close]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/session/tmux/tmux.go#L423-L450
[squad-pause]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/session/instance.go#L422-L506
[squad-kill]: https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/session/instance.go#L287-L311
