# agentctl — public related work

Working research for an unsettled design.

Reviewed 2026-09-17. This comparison informs the proposed `agentctl` interface;
it does not describe an already implemented refactor. The problem is to let a
person or coordinator manage long-lived coding-agent sessions, send work, and
observe results while preserving direct human access to those sessions.

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
with visible native sessions where supported. That scope still needs to earn
its additional implementation cost. Happy should be evaluated as an alternative
or a possible backend before introducing a second remote account/server system.

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

**Implication:** a channel needs a supported path into an active harness and a
path for replies. It does not inherently need an agent supervisor or terminal
multiplexer. A Claude channel backend could avoid terminal delivery, while
remaining specific to Claude's notification protocol and startup requirements.

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

**Implication:** this is a candidate harness backend for a chat client or agent
supervisor. Typed events provide a better basis for replies and turn completion
than rendered terminal text. It is an integration API, not a ready-made chat
bridge. A backend promising simultaneous TUI and remote access must separately
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

## Boundaries to carry into the design

These precedents support separating three choices: how a caller reaches the
agent, how the harness accepts work and emits results, and how its process is
hosted or displayed. Chat and a coordinator are two callers of session control.
A terminal is one way to host and inspect a session. A single CLI can expose
these choices without making every backend depend on a terminal, chat service,
or hosted control plane.

The next design should establish one session identity and lifecycle vocabulary,
then state each backend's supported capabilities. Human attachment, durable
messaging, native goals, and turn completion must be documented and verified
individually. This comparison does not establish that any existing project
implements that entire combination, or that a new implementation is preferable
to extending one of them.
