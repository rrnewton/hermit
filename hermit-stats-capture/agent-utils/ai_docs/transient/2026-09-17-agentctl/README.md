# agentctl — control coding agents while keeping their terminals accessible

You are working with a lead coding agent and want to delegate a review, a test
investigation, or a longer implementation task to another agent. That worker
needs its own conversation and tools. You want the lead to send follow-up work,
learn when a turn finishes, and inspect the result. You also want to open the
worker's terminal yourself, see what it is doing, and give it instructions.

`agentctl` is the proposed common interface for that job. A human, a shell
script, or a coordinator agent would use the same named sessions and operations.
The coordinator continues to decide what work to delegate; each worker continues
to use its native coding harness. Optional chat access lets a human message a
session from a chat application.

**Status: design brief, not an installed command.** The current implementation
is exposed through `herdr-agent`, `herdr-subagents`, and `herdr-chat`. This brief
defines their common problem before their interfaces and state are consolidated.

## What it should make easy

- Start a worker in a chosen directory with a chosen harness, then address that
  same conversation by name across later commands.
- Send work without losing it when the worker is busy or blindly repeating it
  after an uncertain delivery.
- Inspect progress, receive a correlated final answer, and distinguish an idle
  terminal from a completed turn or goal.
- Open an interactive worker's real terminal for direct inspection and input,
  with an explicit way to coordinate human and automated input.
- Connect a chat bridge to one session without requiring a team of workers.

An interactive worker keeps a native TUI running. A headless worker accepts
structured turns and retains a conversation between harness invocations. These
are different execution modes with different capabilities. Terminal hosting,
such as Herdr or tmux, is a separate choice from the harness, such as Codex or
Claude. A chat application is another input/output surface.

## What works today

| Need | Current command | Current limitation |
| --- | --- | --- |
| Native Codex or Claude workers that a human can inspect directly | [`herdr-agent`](../../../common/docs/herdr-run/AGENT_USER_GUIDE.md) | Herdr hosts the interactive terminals. Native goal inspection depends on harness support. |
| Resumable Codex or Antigravity turns, transcripts, and MCP access | [`herdr-subagents`](../../../common/docs/herdr-run/FOREIGN_USER_GUIDE.md) | Headless workers use Herdr or tmux; its interactive mode supports Codex on Herdr only. |
| Google Chat access to one existing coordinator | [`herdr-chat`](../../../common/docs/herdr-run/CHAT_USER_GUIDE.md) | The coordinator must run in Herdr. The bridge submits terminal input and uses an explicit final-reply artifact. |
| Execute a shell command in a Herdr pane and collect its output and exit status | [`herdr-run`](../../../common/docs/herdr-run/rendered/python/USER_GUIDE.md) | Separate shell-execution utility; it does not manage agent conversations. |

These workers currently run on the same host and share local state files with
their controller. Chat provides remote human access to that host's coordinator;
the worker APIs do not provide a multi-host execution service. The caller chooses
working directories and is responsible for coordinating concurrent file changes.

Read the [component design](DESIGN.md) for the exact current message path,
ownership boundaries, proposed interfaces, and limitations. Read
[related work](RELATED_WORK.md) for public open-source alternatives, including
systems that already offer remote session control and agent-to-agent operation.

The follow-up sanity check examines implementation guarantees rather than CLI
feature lists:

- [Our worker lifecycle](CURRENT_LIFECYCLE_REVIEW.md): reproduced failure cases,
  source-level risks, and existing safeguards.
- [Public lifecycle implementations](PUBLIC_LIFECYCLE_REVIEW.md): Happy and
  Claude Squad source comparisons, with pinned references and bounded claims.
- [Dependencies and Chat lifecycle](DEPENDENCIES_AND_CHAT_REVIEW.md): why the
  agent package need not depend on the shell executor, plus local Chat
  reproductions.

The proposed operator documentation surface is CLI `quickstart`, `userguide`,
and top-level/per-command help. These working files are design input, not a
parallel user manual.
