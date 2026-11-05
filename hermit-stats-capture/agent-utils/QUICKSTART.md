# agent-utils quickstart

This walks through adopting the agent-utils suite for a project, using a real run on a
one-commit repository called `foobar`. You do not run the steps yourself: your coding agent does,
following the [`agent-utils-setup`](skills/agent-utils-setup/SKILL.md) skill. This page shows
what to expect, what you will be asked, and what you end up with. For how the pieces fit
together, see [the suite](README.md#the-suite) or the
[website](https://rrnewton.github.io/agent-utils/).

## Before you start

- A Linux machine.
- A coding agent harness you are logged in to: Claude Code, Codex, or Muse.
- Git and Python 3.10 or newer.
- [Herdr](https://github.com/herdrdev/herdr), the terminal server every agent runs in. If it is
  missing, the setup agent offers to install it; start it once with `herdr`.

Nothing has to be built. The suite tools run from an agent-utils checkout on their Python
engine.

## 1. The project

The example project is a one-commit repository whose `origin` is a local bare repository, so
nothing leaves the machine:

```sh
mkdir -p ~/work/util-suite-test/origin
git init --bare -b main ~/work/util-suite-test/origin/foobar.git
git clone ~/work/util-suite-test/origin/foobar.git ~/work/util-suite-test/foobar
cd ~/work/util-suite-test/foobar
printf '# foobar\n' > README.md
git add README.md && git commit -m 'foobar: initial commit' && git push -u origin main
```

Your own project works the same way: start from its ordinary Git checkout.

## 2. Ask your agent

In Herdr, open a tab in the checkout, start your agent (`claude`, `codex`, or `muse`), and say:

> Adopt agent-utils for this project, following `skills/agent-utils-setup/SKILL.md` from
> https://github.com/rrnewton/agent-utils.

That sentence is the whole entry point. The agent reads the skill from GitHub, and later clones
agent-utils into the harness it builds (or adds it as a submodule, if you prefer), so every
project carries its own copy of the tools and skills. (The recorded run pointed the agent at a
local agent-utils checkout instead of the URL; everything else was the same.)

## 3. What it detects

The agent looks before it asks. In the run, it reported the project (`foobar`, clean, on `main`,
`origin` a local bare repository, no linked worktrees or submodules) and the host, then showed a
dependency table:

| Dependency | Needed for | Found |
|---|---|---|
| Git | everything | 2.53.0 |
| Python 3.10+ | the tools themselves | 3.12.14 |
| Herdr | agentctl and the chat bridge (agents run in Herdr tabs) | 0.8.0 |
| Agent harness | the coordinator and subagents | claude, codex, muse |
| User systemd and cgroup v2 | per-slot resource limits, dagrun | yes |
| Passwordless sudo, or `/dev/fuse` with `fuse2fs` | disk-image slots | both |
| Unprivileged user namespaces | the optional file-system sandbox | yes |
| `cargo`, `podman`, `gh` | optional tools | all present |

Nothing was missing, so nothing was installed. When something is missing, the agent says what
needs it, suggests how to install it (checking the upstream instructions first), and installs it
only if you agree.

## 4. The questions

The agent then asked everything it could not work out, in one batch, with a recommended default
for each. Answering "defaults" accepts them all. The run answered:

> Defaults, except: 4) worker on Claude and reviewer on Codex, both interactive, nothing pinned.
> Optional tools: set up herdr-run now, allowing git and gh with the with-proxy prefix.

| # | Question | Default | Run's answer |
|---|---|---|---|
| 1 | Name of the primary checkout folder | `foobar/` (the repository name) | default |
| 2 | Track the harness in its own Git repository? | yes, local only | default |
| 3 | Where agent-utils comes from | clone it into the harness | default |
| 4 | Harnesses and launch profiles | `coordinator`, `worker`, and `reviewer`, all on the current harness, nothing pinned | `worker` on Claude, `reviewer` on Codex |
| 5 | Herdr workspace for the agents | `foobar-agents` | default |
| 6 | Maximum active slots | 4 | default |
| 7 | Slot storage | sparse disk images (sudo or `fuse2fs` available) | default |
| 8 | File-system sandbox for subagents | off: per-slot resource limits only | default |
| 9 | Build caches wrkslots may reclaim | none (no build files found) | default |
| 10 | Chat bridge | later | default |
| | Optional tools now? | none | `herdr-run`, allowing `git` and `gh` |

Launch profiles are your policy: the agent writes exactly what you say and never invents models,
flags, or permission settings.

## 5. The plan, then the work

Before touching anything, the agent listed every move, file, and command and waited for a yes.
After approval it:

1. moved the checkout into the harness: `foobar/` became `foobar/foobar/`, unchanged;
2. cloned agent-utils into `foobar/agent-utils/` and linked its dispatchers as `agent-utils/bin`;
3. created the harness repository and its `.gitignore`;
4. initialized wrkslots with disk-image slots, `cgroup` isolation, and the agentctl-backed
   liveness check in `bin/agent-liveness`;
5. wrote the agentctl launch profiles;
6. linked the suite skills under `.agents/skills/` (and `.claude/skills`);
7. wrote `AGENTS.md` from the harness template, with `CLAUDE.md` linked to it;
8. initialized herdr-run with `git` and `gh` allowed and its own workspace, `foobar-cmds`;
9. committed the harness, then ran read-only checks of every tool.

## 6. The result

```text
~/work/util-suite-test/foobar/      the harness; the coordinator runs here
  AGENTS.md                         "You are the coordinator for foobar..." plus layout, tools,
  CLAUDE.md -> AGENTS.md              how to delegate, and the suite configuration
  foobar/                           the primary checkout, as it was
  agent-utils/                      the suite: tools in bin/, skills in skills/
  worktrees/                        wrkslots: slots/<slot>/, its registry, the wrkslots command
  bin/agent-liveness                tells wrkslots whether a slot's agent has stopped
  .agents/skills/                   agent-utils-coordinator, agent-utils-setup, agentctl,
  .claude/skills -> ../.agents/skills   wrkslots, herdr-run
  .agentctl/profiles.json           coordinator and worker (claude), reviewer (codex); private
  .wrkslots.yml                     slot policy, commented key by key
  .herdr-run.yaml                   allowlist: git, gh
```

The harness repository has one commit with those files. The checkouts, the slot registry, and
agentctl's private state are ignored. The end of `AGENTS.md` records the choices, so a later agent
(or you) can see what is installed:

```markdown
## Suite configuration

- **agentctl**: Herdr workspace `foobar-agents`; profiles: `worker` (Claude Code, interactive),
  `reviewer` (Codex, interactive); no model, effort, or flags pinned.
- **wrkslots**: up to 4 slots, disk-image (sparse ext4, `auto` backend) storage, `cgroup` isolation.
- **Chat bridge**: deferred; the owner talks to the coordinator in its Herdr tab.
- **Optional tools installed**: `herdr-run` (allow `git`, `gh`; prefix `with-proxy`; workspace `foobar-cmds`).
- **Available on request**: `tick-hub`, `dagrun`, `wrkviz`, `gh-paced`, `vibe-talk`, `pr-landing-planner`.
```

## 7. The smoke test

The agent offered one throwaway round trip through the whole delegation path, and ran it:

1. `wrkslots create smoke` made a disk-image slot on branch `smoke/2026-10-06`.
2. `agentctl start smoke --profile worker --slot smoke` started Claude Code in its own Herdr tab,
   inside the slot.
3. The subagent bound itself as the slot's owner, ran `git status` and `pwd`, recorded a handoff,
   and gave the slot back.
4. The setup agent read the handoff, stopped the subagent, and removed the slot. wrkslots
   reported zero active slots and a consistent registry.

## 8. The coordinator, and its first task

Setup ends by starting the coordinator as a registered agentctl session named `coordinator`, with
the `coordinator` launch profile, in the project's Herdr workspace:

```sh
cd ~/work/util-suite-test/foobar
agent-utils/bin/agentctl start coordinator --cwd . --profile coordinator --brief "..."
```

Switch to it in Herdr (workspace `foobar-agents`, tab `coordinator`), or run
`agent-utils/bin/agentctl attach coordinator` from the harness. It reads `AGENTS.md` and the
coordinator skill by itself. Registration matters: it is how timers, scripts, and other agents
reach the coordinator (`agentctl send coordinator`). (This step was added after the recorded
run, which ended by telling the owner to start the coordinator by hand; it was then tested on the
same harness.)

In the test, the coordinator was asked:

> Please have a subagent add CONTRIBUTING.md to foobar containing one line: 'Send patches to the
> owner by email.' Land it on foobar's main branch; foobar has no rules of its own, so a direct
> push to origin main is fine. Tell me when it's done.

It created a slot, started a subagent in it, and let the subagent commit and push. It then read
`main:CONTRIBUTING.md` back from the remote to check the content byte for byte, read the handoff,
stopped the subagent, removed the slot, and fast-forwarded the primary checkout. Its report named
the commit, the check it made, and one leftover (the subagent's pushed working branch), and asked
whether to delete it.

## 9. Later

Change the setup by asking the coordinator. In the test, the coordinator was asked to set up
tick-hub for an hourly status reminder. From the setup skill's templates it wrote `ops.yaml` and
`bin/tick-deliver`, installed a user systemd timer that runs one tick every 15 minutes, and
triggered it once. The reminder arrived in its own queue through `agentctl send coordinator`,
and it answered with a status summary. It then recorded tick-hub in `AGENTS.md` and committed
the harness. Other requests work the same way:

- "Connect this to Google Chat." (It needs a space ID, your user ID, and an OAuth token or a
  command that prints one; `agentctl chat quickstart` lists the details.)
- "Turn on the file-system sandbox for subagents."
- "Add dagrun for our test suite."
- "Update agent-utils."

The coordinator follows the setup skill's "Add a tool later" section and records each change in
`AGENTS.md`.

## Things you may meet

- **"Trust this folder?"** Claude Code may ask once for a new slot folder. agentctl stops the
  launch with "requires human attention"; answer the prompt in the agent's tab, and the
  coordinator delivers the brief afterwards. Once the harness root is trusted, slots below it
  usually do not ask.
- **A harness that dies at startup inside the file-system sandbox**, often with "Read-only file
  system" for some path. Add that path to `sandbox.read_write` in `.wrkslots.yml`. Some harness
  launchers need a credential directory there, and some need `root` isolation instead of `userns`
  because they perform a setuid step.
- **herdr-run refusing to open a tab** because a shared workspace is at its `max_panes` cap. Give
  the project its own workspace label in `.herdr-run.yaml`, as the run did.
- **Slot removal refusing.** Removal waits until nothing uses the slot and, for a sandboxed
  agent, until the slot's time-to-live has passed. The refusal names the condition. Never delete
  a slot by hand.
- **`agentctl wait` returning at once.** It reports readiness for input, not that the agent
  finished; read the agent's reply with `agentctl read`.
