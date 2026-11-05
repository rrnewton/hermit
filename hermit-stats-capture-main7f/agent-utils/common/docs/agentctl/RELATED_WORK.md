# agentctl — public related work

Reviewed 2026-09-17. `agentctl` lets a person or coordinator manage long-lived
coding-agent sessions, send work, and observe results while preserving direct
human access. This comparison explains the relationship to other public systems.
The installed `agentctl capabilities` command identifies available adapters and
extensions; interactive Herdr control is the shared core.

The sources below are public open-source projects. Repository documentation,
implementation files, and licenses were checked at the linked revisions. These
are source comparisons, not end-to-end interoperability tests. Hosted services
and the coding agents used by these projects have their own terms; the licenses
listed here cover the linked software.

| Precedent | Control boundary | Relevant lesson |
| --- | --- | --- |
| Happy and `happy-agent` | A session wrapper, remote clients, and a synchronization server | Remote session control already includes an agent-facing CLI. |
| Claude Code Discord and Telegram plugins | Chat service → MCP channel notification → Claude session | Messaging can enter the harness without terminal input. |
| Codex app-server | A client sends structured thread/turn requests to the harness | Session control and result events need not depend on a terminal. |
| Claude Squad | A terminal dashboard manages agents in tmux and Git worktrees | Human attachment and workspace management are separate concerns from chat. |

## Happy: the closest remote-control comparison

[Happy](https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/README.md)
provides `happy claude` and `happy codex` wrappers, mobile/web clients, and a
synchronization server. Its README describes switching from local interaction
to remote mode by restarting the session, then returning control to the keyboard.
Conversation continuity should therefore not be equated with keeping the exact
same TUI process alive throughout the handoff.

More directly relevant,
[`happy-agent`](https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/packages/happy-agent/README.md)
already exposes machine discovery, session spawning, listing, status, messaging,
history, waiting for idle, and stopping. It can select Codex when spawning and
offers JSON output and `send --wait`. A coordinator can use this interface;
Happy is not only a phone interface for a human. Its documented setup uses Happy
account authentication and the synchronization service.

**Implication:** another remote-control CLI is not a novel contribution.
The narrower case for `agentctl` is a common local interface over the existing
interactive and headless workers, durable delivery, and optional chat access,
with visible native sessions where supported. It does not require a second
remote account or synchronization server. Happy remains an alternative when
its hosted or self-hosted remote clients are the desired interface.

License: [MIT](https://github.com/slopus/happy/blob/3fd0be9e2afb19cce67fed40af379db5e73b7d27/LICENSE).

## Claude Code channels: chat connects to the harness

The public
[Discord](https://github.com/anthropics/claude-plugins-official/blob/1aa8f02ec8327f513686934f458a620f83db91ed/external_plugins/discord/README.md)
and
[Telegram](https://github.com/anthropics/claude-plugins-official/blob/1aa8f02ec8327f513686934f458a620f83db91ed/external_plugins/telegram/README.md)
plugins run MCP servers alongside a Claude Code session launched with
`--channels`. They receive messages from their chat service and expose reply,
reaction, and edit tools to the agent. The comparison is to these open-source
plugins, not a claim that Claude Code itself is open source.

The Discord
[implementation](https://github.com/anthropics/claude-plugins-official/blob/1aa8f02ec8327f513686934f458a620f83db91ed/external_plugins/discord/server.ts)
delivers `notifications/claude/channel` with message content and sender/thread
metadata. It does not type the message into a terminal. Its
[access configuration](https://github.com/anthropics/claude-plugins-official/blob/1aa8f02ec8327f513686934f458a620f83db91ed/external_plugins/discord/ACCESS.md)
supports pairing, sender allowlists, opted-in group channels, mention patterns,
and a configurable acknowledgment reaction. The implementation attempts the
reaction before notifying Claude; this is a best-effort receipt indicator, not
proof that the agent completed the request.

**Comparison:** a channel needs a supported path into an active harness and a
path for replies. It does not inherently need an agent supervisor or terminal
multiplexer. The current `agentctl chat` extension instead targets a coordinator
in Herdr through terminal input. It persists authorized intake and a configurable
reaction ACK separately from the explicit final-reply outbox. It does not
implement the Claude channel protocol or cross-space trigger discovery.

Licenses: Apache-2.0 for
[Discord](https://github.com/anthropics/claude-plugins-official/blob/1aa8f02ec8327f513686934f458a620f83db91ed/external_plugins/discord/LICENSE)
and
[Telegram](https://github.com/anthropics/claude-plugins-official/blob/1aa8f02ec8327f513686934f458a620f83db91ed/external_plugins/telegram/LICENSE).

## Codex app-server: structured session control

Codex's public
[app-server protocol](https://github.com/openai/codex/blob/16f49ccd7f72b158eebe18b641005b038e7f2df2/codex-rs/app-server-protocol/src/protocol/common.rs)
defines thread start/resume, turn start/steer/interrupt, completion and message
notifications, and explicit approval requests. Its
[server entry point](https://github.com/openai/codex/blob/16f49ccd7f72b158eebe18b641005b038e7f2df2/codex-rs/app-server/src/main.rs)
accepts protocol connections independently of a terminal UI.

**Comparison:** structured events provide a useful basis for replies and turn
completion. This is an integration API, not a ready-made chat bridge. The
current `agentctl` interactive adapter uses Herdr and does not claim to adopt
arbitrary live Codex app-server sessions. A backend promising simultaneous TUI and remote access must separately
prove that both clients address the intended live runtime; matching a saved
conversation identifier alone is insufficient verification.

License: [Apache-2.0](https://github.com/openai/codex/blob/16f49ccd7f72b158eebe18b641005b038e7f2df2/LICENSE).

## Claude Squad: visible workers and isolated workspaces

[Claude Squad](https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/README.md)
manages several coding-agent programs through a TUI. It uses tmux for terminal
sessions and Git worktrees for separate branches. Users can attach to a worker,
reprompt it, detach, inspect changes, and resume or remove sessions.

**Implication:** native terminal visibility is a useful requirement in its own
right, and tmux can host interactive workers. Whether a supervisor can reliably
deliver prompts or observe turns is a separate harness-integration question.
Worktree isolation is another optional layer; neither chat nor agent messaging
should require it merely because a dashboard offers it.

License: [AGPL-3.0](https://github.com/smtg-ai/claude-squad/blob/ce1ffb4392b01f38e2c4599c7c84d2a93973b138/LICENSE.md).

## Coordinator inbox: telling a supervisor that workers need it

Reviewed 2026-09-29 for `agentctl inbox`. These systems solve the reverse
direction of prompt delivery: letting a supervising agent learn that workers
went idle, got stuck, or reported, without polling every worker and without
several writers typing into the supervisor at once. Sources were read from
package-registry snapshots, not from the live repositories, and the licenses
were not re-checked for this section.

- [Gas Town](https://github.com/steveyegge/gastown) (Go module snapshot at
  commit `649b832b7672`, 2026-07-23). `gt nudge` waits until the target pane
  shows an idle prompt on two polls 200 ms apart. After 15 s it falls back to
  a file queue, one timestamp-named JSON file per nudge, claimed by atomic
  rename, with a TTL and a depth cap. A Claude hook drains that queue at the
  next turn boundary, rendering everything as one block with urgent items
  first. Stuck detection is deliberately slow (30 minutes without progress),
  because false positives cost more than late detection.
- [CLI Agent Orchestrator](https://github.com/awslabs/cli-agent-orchestrator)
  (PyPI 2.5.0). Inbox messages stay pending in a database until the receiver's
  terminal is idle or completed. An immediate attempt, a log watchdog, and a
  30-second reconciliation sweep all feed the same gate; the sweep exists
  because an already-idle receiver produces no output to trigger the others.
  Messages are marked delivered before the paste, so a crash cannot deliver
  twice.
- Claude Code's own cross-session messaging, teammate mailboxes and channels
  (public documentation at `code.claude.com/docs`). Messages are queued
  natively and read between tool calls, or start a turn when the receiver is
  idle. Several channel events arriving during one turn are handed over
  together.

**Implication:** the inbox adopts the shared lessons: deliver as one batch in
priority order, keep one live state per worker instead of a history of
transitions, make retries idempotent by batch, and keep the queue on disk so
that a restart loses nothing. It deliberately stores notices only. Detecting
idle workers and choosing work for them stay with the caller: a watcher, the
coordinator, or a person.

## Lifecycle guarantees and component boundaries

`agentctl` separates the caller (CLI, MCP, or Chat), the named-session manager,
and the adapter that hosts and addresses the harness. It does not depend on the
`herdr-run` shell executor. Interactive sessions retain a native Herdr terminal;
the worker extension supports resumable headless turns with a terminal transcript
view. These are different capabilities, not interchangeable kinds of TUI access.

Source inspection does not establish production reliability. In the pinned
Happy source, direct `happy-agent` messaging and wrapper messaging have different
acknowledgement paths; session presence and process termination are separate
operations. Claude Squad uses terminal text stability as a readiness signal.
Those details are reasons to inspect a particular code path, not to borrow a
project-wide guarantee from a feature list.

For `agentctl`, acceptance, readiness, native goal completion, process identity,
and human input ownership are separate facts. Its durable queues retain uncertain
submission for inspection rather than automatically replaying it. Native goal
inspection requires a bound supporting harness. The Chat ACK reports durable
intake and can precede a much later answer; it does not prove completion.

The public comparisons above do not establish that one system implements every
combination of local control, mobile access, terminal visibility, and protocol
adoption. Choose the system whose actual control boundary fits the workflow.
