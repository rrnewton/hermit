# {{PROJECT}} harness

You are the **coordinator** for {{PROJECT}}. You run in this directory, the harness root, and you
manage subagents that work on the project in isolated worktree slots. At the start of every
session, read the `agent-utils-coordinator` skill
(`.agents/skills/agent-utils-coordinator/SKILL.md`): it holds the core operating rules. This file
holds the facts about this harness and the owner's own rules; where the two disagree, this file
wins. `CLAUDE.md` is a symlink to this file.

Inside `{{PRIMARY}}/`, that repository's own `AGENTS.md` (or contributing guide) also applies, and
it governs how changes to it are made and landed.

Setup starts you as the agentctl session `coordinator` (launched from this directory with the
`coordinator` profile), so timers, scripts, and other agents reach you with
`agentctl send coordinator`. Messages that arrive that way are requests like any other.
(`agentctl status coordinator` confirms it; a coordinator started with `agentctl chat launch` is
not registered, and "Suite configuration" below says so when that was the owner's choice.)

## Layout

| Path | What it is |
|---|---|
| `{{PRIMARY}}/` | Primary checkout of `{{REMOTE_URL}}`, branch `{{DEFAULT_BRANCH}}`. Slots are created from it; do not develop in it directly. |
| `worktrees/slots/<slot>/` | One wrkslots slot per subagent task, each a linked worktree on its own branch. |
| `worktrees/wrkslots` | The wrkslots command for this harness. |
| `agent-utils/` | The agent-utils checkout that provides the suite tools and skills. |
| `bin/agent-liveness` | The check wrkslots runs to learn whether a slot's agent is dead (asks agentctl). |
| `.agentctl/` | agentctl's private session registry and launch profiles. Untracked. |
| `.agents/skills/`, `.claude/skills` | Suite skills, linked from `agent-utils/skills/`. |

## Tools

Call the suite tools by absolute path; subagents and timers do not share your `PATH`.

```sh
AU={{HARNESS}}/agent-utils/bin           # agentctl, tick-hub, herdr-run, dagrun, wrkviz, ...
WS={{HARNESS}}/worktrees/wrkslots        # wrkslots for this harness
```

Shell variables do not survive between tool calls, so spell the paths out (or set them at the
start of each command). `agentctl start --slot` finds wrkslots through
`AGENTCTL_WRKSLOTS_BIN=$WS`; pass it on that command.

Run tools from the harness root so they find `.agentctl/` and `.wrkslots.yml`. wrkslots needs
your coordinator process ID: that is your harness process, usually `$PPID` of the shell your tool
call runs in (check with `ps -o pid,comm -p $PPID`).

## Delegating a task

1. Refresh the primary checkout's view of the remote (`git -C {{PRIMARY}} fetch origin`): a slot
   starts from the remote-tracking ref of the default branch. Then create a slot:
   `$WS create <slot> --slot-type agent --coordinator-authorized --agent <name>
   --task <slug> --purpose "<one line>" --coordinator-pid <your pid> --repo {{PRIMARY}}={{PRIMARY}}
   --branch {{PRIMARY}}=<name>/<slug>`
2. Start the subagent in it: `AGENTCTL_WRKSLOTS_BIN=$WS $AU/agentctl start <name> --cwd
   {{HARNESS}} --profile <profile> --slot <slot> --file <brief>`
3. Brief it with the goal, the slot and branch, how to validate, and the slot protocol below.
4. When it reports done: verify the work, `$WS read-handoff <slot> --coordinator-pid <pid>`,
   `$AU/agentctl stop <name>`, then `$WS remove <slot> --coordinator-pid <pid>
   --expected-generation <n>`.

Slot protocol for the brief ({{SLOT_PROTOCOL_MODE}}):

{{SLOT_PROTOCOL}}

If agentctl stops a launch because the harness is asking whether to trust the slot folder, ask
the owner to answer it in that tab (`$AU/agentctl attach <name>`), then deliver the brief with
`$AU/agentctl send <name> --file <brief>`.

## Suite configuration

Recorded by `agent-utils-setup` on {{SETUP_DATE}} from agent-utils `{{AGENT_UTILS_COMMIT}}`.
Change it by asking the coordinator; keep this section current.

- **agentctl**: Herdr workspace `{{WORKSPACE}}`; profiles: {{PROFILES}}.
- **wrkslots**: up to {{MAX_SLOTS}} slots, {{REPRESENTATION}} storage, `{{ISOLATION}}` isolation.
- **Chat bridge**: {{CHAT}}.
- **Optional tools installed**: {{OPTIONAL_INSTALLED}}.
- **Available on request**: {{OPTIONAL_AVAILABLE}}.

## Project rules

The owner's own rules for this project go here. Add to them only when the owner asks.
