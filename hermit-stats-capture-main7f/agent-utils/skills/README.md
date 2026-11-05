# agent-utils skills

These skills are designed as one set. Two of them describe the suite as a whole: how to adopt it
for a project, and how the coordinator agent works once it is adopted. The rest each cover one
tool, and point to that tool's installed `quickstart`, `--help`, and embedded user guide instead of
copying its manual.

## Suite skills

- `agent-utils-setup` — adopt the suite for a project through a guided, wizard-style flow:
  dependencies, the harness layout, agentctl profiles, wrkslots, the chat bridge, optional tools,
  and the harness `AGENTS.md` (from [`agent-utils-setup/templates/`](agent-utils-setup/templates/)).
  Also the place to start when adding a tool later.
- `agent-utils-coordinator` — the core operating rules for the coordinator: delegate through
  wrkslots and agentctl, supervise and verify, land under the project's own rules, keep durable
  records, and report substance to the owner.

## Core tool skills

- `agentctl` — start, inspect, message, and retire named agents, including owner-configured
  profiles.
- `agentctl-chat` — answer the chat requests the agentctl chat bridge types into an agent's
  pane, between the prompt's marker lines, and check what reached the chat.
- `wrkslots` — provision and audit isolated Git worktree slots, protect or reclaim their caches,
  record handoff, and remove source only after verified owner absence.

## Optional tool skills

- `herdr-run` — run an allowlisted command in a Herdr pane, outside whatever constrains the
  caller; a sandboxed agent's blocked `git` is one case, not the definition.
- `tick-hub` — evaluate recurring reminders and health checks from one scheduled tick.
- `dagrun` — schedule a dependency graph under CPU, memory, and named-resource limits.
- `cpuset-alloc` — reserve disjoint host CPU sets with stale-owner reclamation.
- `parallel-experiment-runner` — run boxed, resource-bounded concurrent seed sweeps.
- `wrkviz` — archive and visualize coordinator and subagent activity.
- `pr-landing-planner` — produce an advisory, machine-readable PR landing plan.
- `pr-landing-operations` — validate and execute an authorized landing plan safely; a process
  guide for authorized repository operators.
- `herdr-agent` — durably deliver and inspect prompts for an interactive agent in a Herdr pane
  (the older interface; start new integrations with `agentctl`).

## Install in a harness

`agent-utils-setup` links the skills a project uses into its harness, once for every agent
harness:

```sh
mkdir -p .agents/skills .claude
ln -s ../../agent-utils/skills/agent-utils-coordinator .agents/skills/agent-utils-coordinator
ln -s ../../agent-utils/skills/wrkslots .agents/skills/wrkslots
# ...one link per skill...
ln -s ../.agents/skills .claude/skills
```

Codex reads `.agents/skills`, and Claude Code reads `.claude/skills`. Because they are links, a
`git pull` in `agent-utils/` updates every skill at once.

For `agentctl` alone, `agentctl skill install` installs byte-identical copies into the user-level
Codex and Claude skill directories and uses Muse's native managed skill installer. It refuses
divergent content unless explicitly forced.

Tool commands must be reachable: in a source checkout, the tracked dispatchers in `common/bin/` run
the Python engine with no build step. `./setup` builds and checks both editions for development and
links them as `./bin`; a consumer that skips the build can create that link itself
(`ln -s common/bin bin`). Landing operations additionally require authorization from the consuming
repository's own rules.
