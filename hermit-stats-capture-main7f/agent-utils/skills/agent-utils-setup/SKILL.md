---
name: agent-utils-setup
description: Adopt the agent-utils suite for a project through a guided, wizard-style conversation - detect what can be detected, install missing dependencies such as Herdr with the owner's consent, turn a Git checkout into a coordinator harness (primary checkout, worktrees, agent-utils, skills, harness AGENTS.md), and configure agentctl, wrkslots, and optionally the chat bridge and other suite tools. Use when the owner says "adopt agent-utils for my project", "set up agent-utils here", "set up agentutils", or "set up agent utils", or later asks to add, remove, or reconfigure a suite tool.
---

# Adopt agent-utils for a project

You are guiding the owner through setup. This is a conversation, not a script:

- Work through the phases in order. **Answer everything you can detect yourself**, say what you
  detected, and ask the owner only what you cannot know.
- Ask in small batches (at most about five questions per phase), each with a recommended default.
  "Defaults" is a valid answer to the whole batch.
- **Change nothing on disk until the owner approves the plan in phase 4.** Phases 0 to 3 only read.
- Keep a running record of the answers. It becomes the "Suite configuration" section of the
  harness `AGENTS.md`.
- Installation hints below are starting points that age. Check the upstream page named with each
  one before running it, and prefer what it says now.

Throughout, `AU_SRC` is the agent-utils source the owner pointed you at (a local path or a Git
URL; the public repository is `https://github.com/rrnewton/agent-utils`), `CHECKOUT` is the
absolute path of the project checkout you were started in, and `H` is the harness root. If the
owner gave only the URL, read this skill from the repository first: fetch the raw file, or clone
the repository to a temporary directory outside the checkout, which can then serve as `AU_SRC`.
The real copy is cloned into the harness in step 5.2.

## What setup produces

The checkout the owner is in becomes the **harness**: a directory that holds the project's primary
checkout plus everything the coordinator needs. For a repository `foobar` checked out at
`~/work/foobar`:

```text
~/work/foobar/                  harness root (H); the coordinator runs here
  AGENTS.md, CLAUDE.md -> AGENTS.md
  foobar/                       primary checkout, moved here unchanged
  worktrees/                    wrkslots: slots/<slot>/ checkouts, registry, wrkslots command
  agent-utils/                  agent-utils checkout: tools in bin/, skills in skills/
  bin/agent-liveness            liveness check wrkslots calls
  .agents/skills/               links to suite skills; .claude/skills points here
  .agentctl/                    agentctl registry and launch profiles (private, untracked)
  .wrkslots.yml                 wrkslots configuration
```

The harness is its own small Git repository (local only unless the owner adds a remote), so its
configuration and rules are versioned. The primary checkout stays an independent repository;
the harness ignores it.

**Core tools**, always set up: `agentctl` (named agents in Herdr tabs), `wrkslots` (one isolated
worktree slot per agent), and the **chat bridge** (`agentctl chat`, so the owner can reach the
coordinator from chat; it may be deferred). **Optional tools** are offered, not pushed:
`herdr-run`, `tick-hub`, `dagrun`, `wrkviz`, `gh-paced`, `vibe-talk`, `pr-landing-planner`.

## Phase 0: Orient (detect; ask nothing)

Collect and then summarize in a few lines:

- **The project.** `git -C . rev-parse --show-toplevel` gives `CHECKOUT`. Note the repository name
  (directory name, or the remote's last path component), `git remote -v`, the default branch
  (`git symbolic-ref --short refs/remotes/origin/HEAD`, else the current branch), uncommitted
  changes (`git status --porcelain`), linked worktrees (`git worktree list`), and submodules.
  If the current directory is not in a Git checkout, ask whether to clone one or create one; the
  rest of this flow assumes a checkout.
- **Already a harness?** If `CHECKOUT` (or a parent) has `.wrkslots.yml` and an `AGENTS.md` that
  names `agent-utils-coordinator`, setup already ran: go to "Add a tool later" instead.
- **Agent harnesses on `PATH`**: `claude`, `codex`, `muse`. Which one are you running in?
- **Host capabilities**, for the dependency table and the wrkslots questions:
  `herdr --version`, `python3 --version`, `cargo --version`, `git --version`, `gh --version`,
  `podman --version`; `systemctl --user is-system-running` (a user systemd manager);
  `stat -fc %T /sys/fs/cgroup` (`cgroup2fs` means cgroup v2); `sudo -n true` (passwordless sudo);
  `/dev/fuse` and `fuse2fs` (unprivileged disk images); whether unprivileged user namespaces
  work. Some sandboxes refuse a probe; record "unknown" rather than forcing it.
- **Network.** If downloads fail, the host may need a proxy or may block the destination. Say so
  plainly instead of retrying blindly.

## Phase 1: Dependencies

**Always show the owner the phase 0 summary and this dependency table, as text, before asking any
question**, even when nothing is missing: it is how the owner learns what the suite relies on and
what you found. One row per dependency: what it is needed for, and found (with version) or
missing. Offer to install only what is missing, and only with the owner's yes. Prefer user-level
installs; never change host configuration.

| Dependency | Needed for | Hint (verify upstream first) |
|---|---|---|
| Git | everything | Distribution package. |
| Python 3.10 or newer | the tools' default Python engine; no build step | Distribution package. |
| Herdr | agentctl, the chat bridge, herdr-run: every agent runs in a Herdr tab | On Linux and macOS, try the upstream installer, `curl -fsSL https://herdr.dev/install.sh \| sh` (installs to `~/.local/bin`; `HERDR_INSTALL_DIR` overrides), or `brew install herdr`, or a release binary. Confirm against the install section of https://github.com/herdrdev/herdr or https://herdr.dev/docs/ first. agent-utils was last tested with Herdr 0.8; if a newer Herdr misbehaves, report the version. |
| An authenticated agent harness | the coordinator and subagents | Claude Code, Codex, or Muse, per their own install docs. |
| A user systemd manager and cgroup v2 | wrkslots per-slot limits and boxes; dagrun | Present on most current Linux distributions. |
| Passwordless `sudo`, or `/dev/fuse` with `fuse2fs` | disk-image slots (`kernel` or `fuse` backend) | `fuse2fs` is in the e2fsprogs packages of most distributions. |
| Rust toolchain (`cargo`) | optional: Rust editions, `gh-paced`, building `vibe-talk` | https://rustup.rs |
| `podman` or another container runtime | optional: the `vibe-talk` container image (a native build works too) | Distribution package. |
| `gh` | optional: GitHub operations by agents | https://cli.github.com |

After installing Herdr, the owner normally starts it once (`herdr`) so its server is running;
agentctl talks to that server.

## Phase 2: Layout

Show the tree from "What setup produces" with the real names filled in, then ask:

1. **Primary checkout directory name.** Default: the repository name (`foobar/foobar/`), which keeps
   the repository's identity in every path and leaves room for sibling repositories later.
   Alternative: `primary/`.
2. **Track the harness in its own Git repository?** Default: yes, local only.
3. **Where agent-utils comes from.** Default: clone `AU_SRC` into `H/agent-utils`. If `AU_SRC` is
   a local checkout, clone from it and then point `origin` at the public URL (or the owner's fork).
   Alternatives: add it as a Git submodule of the harness repository at `agent-utils/`, which
   records the exact agent-utils commit in the harness history; or use an existing checkout by
   symlink, which shares its updates.

The harness root is `CHECKOUT` itself unless the owner wants a different directory.

## Phase 3: Core tools, then optional tools

Ask as one or two batches:

4. **Harnesses.** Which harness runs the coordinator, and which run subagents (defaults: the one
   you are running in, for both). For each, does the owner want a specific model, reasoning
   effort, or harness flags? These become agentctl **launch profiles**, which are owner policy:
   write exactly what the owner says, and never invent flags, models, or permission settings.
   Always write a `coordinator` profile (setup launches the coordinator with it in phase 7), plus
   profiles for subagents such as `worker` and `reviewer`.
5. **Herdr workspace** for the project's agents. Default: `<repo>-agents`.
6. **How many slots** may be active at once. Default: 4; suggest more only for a large machine.
7. **Slot storage.** Default: disk images when passwordless sudo or `fuse2fs` is available (each
   slot is one sparse ext4 image: a runaway build fills its own image instead of the disk, and
   removal deletes one file); otherwise plain directories. Images are sparse, so the size ceiling
   is not a reservation.
8. **File-system sandbox for subagents.** Default: off, which means each slot still gets its own
   systemd resource limits (`cgroup` isolation) and the agent manages its own slot ownership,
   handoff, and release. On (`userns`, or `root` for harnesses whose launcher needs a setuid step)
   gives every agent a view in which only its slot, its Git directories, and named paths are
   writable. It is an accident boundary, not a security boundary. With it on, a subagent cannot
   write the wrkslots registry, so the coordinator reclaims its slot after `agentctl stop` once
   the slot's time-to-live has passed, using `bin/agent-liveness`. A harness may need extra
   writable paths inside the box (see phase 6). It can be turned on later by asking.
9. **Build caches.** Directories that are regenerable build output (for example `target`,
   `node_modules`, `.venv`, `build`), so wrkslots can reclaim them. Propose a list from the
   project's `.gitignore` and build files; ask the owner to confirm.
10. **Chat bridge.** Connect chat now, or later? The working inbound transport is Google Chat
    (`agentctl chat launch`, polling), which needs a space ID, the owner's user ID for the
    allow-list, and an OAuth access token or a command that prints one. Run `agentctl chat
    quickstart` for the current requirements. Other providers need an installed subscription
    plugin (`agentctl chat --help`). Without chat, the owner talks to the coordinator in its
    Herdr tab.

Then present the optional tools in one short list and ask which, if any, to set up now (default:
none; each can be added later by asking):

- `herdr-run`: run an allowlisted command (such as `git push`) in a visible Herdr pane when the
  agent's own sandbox blocks it; every run is recorded.
- `tick-hub`: one scheduled tick that turns many recurring duties (status reports, freshness
  checks) into lines delivered to the coordinator with `agentctl send`.
- `dagrun`: run the project's validation as a dependency graph locally, under CPU and memory
  limits.
- `wrkviz`: a zoomable timeline of what the coordinator and subagents did, from their transcripts.
- `gh-paced`: a `gh` wrapper that paces calls so many agents sharing one GitHub account stay inside
  GitHub's limits (Rust build).
- `vibe-talk`: a deployable voice and phone front end for chat-bridged sessions (a service with
  its own deployment; see `agent-utils/vibe-talk/README.md`).
- `pr-landing-planner`: an advisory plan for landing a queue of pull requests (experimental).

## Phase 4: Confirm the plan

List every action you will take, with real paths: the move, the clone, each file you will write,
each command you will run, and anything you will install. Get an explicit yes. If the checkout
has uncommitted changes or linked worktrees, say what happens to them (the move keeps both;
linked worktrees are repaired afterwards).

## Phase 5: Execute

Use absolute paths throughout. Stop and report at the first failure; do not improvise around it.

**5.1 Move the checkout into the harness.** With `PRIMARY` the chosen name:

```sh
mv "$CHECKOUT" "$CHECKOUT.adopting"
mkdir "$CHECKOUT"
mv "$CHECKOUT.adopting" "$CHECKOUT/$PRIMARY"
git -C "$CHECKOUT/$PRIMARY" worktree repair      # only matters if it had linked worktrees
git -C "$CHECKOUT/$PRIMARY" status --short --branch
```

Your own working directory moved with the checkout: it is now `$H/$PRIMARY`. Keep using
absolute paths; the coordinator will later start in `$H`.

**5.2 agent-utils.** `git clone "$AU_SRC" "$H/agent-utils"`; for a local source, then
`git -C "$H/agent-utils" remote set-url origin <public or fork URL>`. No build is needed: the
tracked dispatchers in `agent-utils/common/bin/` run the Python engine (each prints one `engine=`
line on stderr). Link them as `agent-utils/bin`, which is what agent-utils' own `./setup` does
after its development build and checks:

```sh
ln -s common/bin "$H/agent-utils/bin"     # agent-utils ignores this path
"$H/agent-utils/bin/agentctl" --version
```

For the submodule choice, run `git -C "$H" submodule add <URL> agent-utils` after 5.3 instead of
the clone, leave `/agent-utils/` out of `.gitignore`, create the same `bin` link, and commit the
submodule with the harness in 5.10. Updating agent-utils is then a submodule update plus a
harness commit.

**5.3 Harness repository.** If chosen: `git init "$H"` and write `$H/.gitignore`:

```gitignore
/PRIMARY/
/agent-utils/
/worktrees/
/.agentctl/
/.herdr-run/
/.wrkslots.yml.bak
/chat.json
```

with `PRIMARY` replaced. agentctl refuses to use launch profiles unless `.agentctl/` is ignored,
so write this file before 5.5 even if the harness is not tracked (then put the rules in
`.git/info/exclude` of whichever repository contains `H`, or create the repository anyway).

**5.4 wrkslots.**

```sh
mkdir -p "$H/bin"
cp "$H/agent-utils/py/wrkslots/examples/agentctl_liveness_probe.py" "$H/bin/agent-liveness"
chmod +x "$H/bin/agent-liveness"
"$H/agent-utils/bin/wrkslots" init "$H" --worktrees-dir worktrees/slots --layout flat \
  --liveness-command bin/agent-liveness --max-active-slots N \
  --slot-representation image|worktree --sandbox-isolation cgroup|userns|root \
  [--cache-glob GLOB ...]
```

`--sandbox-isolation cgroup` means sandbox off; `userns` or `root` means sandbox on. Check with
`"$H/worktrees/wrkslots" sandbox show-config` run from `$H`. With the sandbox on, a slot is
reclaimed only after its time-to-live (`--heartbeat-ttl-seconds`, default one hour) has passed
since it was created; suggest a shorter value, such as 900, if the owner wants slots back sooner.

When wrkslots removes a slot that still holds unpublished commits, it salvages them: to the
remote when that remote is listed with `--salvage-push-remote URL`, otherwise into a verified
local Git bundle under `worktrees/wrkslots-salvage/`. Ask the owner whether salvage may push to
the project's remote; without it, salvaged work stays on this machine.

**5.5 agentctl profiles.** Create `$H/.agentctl` with mode 0700 and
`$H/.agentctl/profiles.json` with mode 0600, owned by the owner, as a regular file (not a link):

```json
{
  "schema": "agentctl-profiles/v1",
  "workspace": "<workspace label>",
  "profiles": {
    "coordinator": {"harness": "claude", "mode": "interactive"},
    "worker": {"harness": "claude", "mode": "interactive"}
  }
}
```

Each profile takes `harness` and `mode`, and optionally `model`, `reasoning_effort`, `argv`, and
`env`, exactly as the owner specified. Verify with `"$H/agent-utils/bin/agentctl" profiles --cwd "$H"`.

**5.6 Skills.** Link the suite skills into the harness, once, for every harness:

```sh
mkdir -p "$H/.agents/skills" "$H/.claude"
for s in agent-utils-coordinator agent-utils-setup agentctl wrkslots; do
  ln -s "../../agent-utils/skills/$s" "$H/.agents/skills/$s"
done
ln -s ../.agents/skills "$H/.claude/skills"
```

Add the skill of each optional tool the owner chose (`herdr-run`, `tick-hub`, `dagrun`, `wrkviz`,
`pr-landing-planner`, `pr-landing-operations`). Codex reads `.agents/skills`; Claude Code reads
`.claude/skills`. Muse manages skills through its own installer.

**5.7 Harness `AGENTS.md`.** Copy `agent-utils/skills/agent-utils-setup/templates/AGENTS.md` to
`$H/AGENTS.md`, replace every `{{PLACEHOLDER}}` from your record, and `ln -s AGENTS.md
"$H/CLAUDE.md"`. For `{{SLOT_PROTOCOL}}`, use the block below that matches the sandbox choice. Fill
`{{AGENT_UTILS_COMMIT}}` with `git -C "$H/agent-utils" rev-parse --short HEAD`. Leave the "Project
rules" section for the owner.

Sandbox off (`cgroup`):

```text
- First bind yourself as the slot's owner: <WS> adopt <slot> --agent <name> --owner-pid $PPID
  --expected-generation <n>   ($PPID of your tool shell is your harness process; check it
  with ps -o pid,comm -p $PPID and reuse that PID below.)
- Work and commit only in your slot, on your branch. Publish the branch as the primary
  repository's rules require.
- When done, write a handoff (what changed, the commit, how it was validated, open questions)
  to a file outside the slot, then run: <WS> write-handoff <slot> --agent <name> --owner-pid
  <pid> --expected-generation <n> --from-file <file>; and <WS> release <slot> --agent <name>
  --owner-pid <pid> --expected-generation <n>.
- Then reply with a short summary. The coordinator stops you and removes the slot.
```

Sandbox on (`userns` or `root`):

```text
- You are boxed: only your slot, its Git directories, and a few named paths are writable, and the
  wrkslots registry is read-only to you. Do not try to adopt, hand off, or release the slot.
- Work and commit only in your slot, on your branch. Publish the branch as the primary
  repository's rules require.
- When done, reply with your handoff: what changed, the commit, how it was validated, and open
  questions. The coordinator stops you and removes the slot after its time-to-live.
```

**5.8 Chat bridge (if chosen now).** Write the configuration `agentctl chat quickstart` describes
to `$H/chat.json` with mode 0600; it is ignored by the harness repository. The token stays in an
environment variable or a token command, never in a tracked file. The coordinator is then
launched with `agentctl chat launch --config chat.json --harness <claude|codex>` from a Herdr
shell tab in `$H` (phase 7). Note the default poll interval in `agentctl chat launch --help` and
ask the owner whether to shorten it.

**5.9 Optional tools.** Follow "Add a tool later" for each one chosen.

**5.10 Commit the harness**, if tracked. Stage the files you wrote by name
(`git -C "$H" add .gitignore AGENTS.md CLAUDE.md .wrkslots.yml bin .agents .claude`, plus any
optional tool's configuration), check `git -C "$H" status --short` shows nothing unexpected, and
commit.

## Phase 6: Verify

Run read-only checks from `$H` and show the results:

- `"$H/agent-utils/bin/agentctl" capabilities` and `"$H/agent-utils/bin/agentctl" profiles --cwd "$H"`
- `"$H/worktrees/wrkslots" status --all-machines` and `"$H/worktrees/wrkslots" sandbox show-config`
- `herdr --version`

Then offer a **smoke test**: one throwaway subagent round trip, which proves the whole delegation
path. Create slot `smoke` on branch `smoke/<date>` with your harness PID as coordinator, start an
agent with the `worker` profile and a brief that follows the slot protocol and only runs
`git status` and `pwd`, read its reply, then read the handoff (sandbox off), stop it, and remove
the slot. Learn that the agent finished from its own reply (`agentctl read <name>` until the
reply is there), not by polling `wrkslots status`: a released slot stays active until you remove
it. `agentctl wait` reports readiness for input, not completion, and can read as idle just after a
prompt was delivered. Expect these on the way, and handle them as described:

- **"workspace trust prompt requires human attention"** (agentctl exit 75): the harness asks
  whether to trust the slot folder. Ask the owner to answer it in the agent's tab
  (`agentctl attach <name>`), then deliver the brief with `agentctl send <name> --file <brief>`
  (or `agentctl drain <name>` when agentctl reports the brief as pending). Once the owner has
  trusted the harness root in a finished session, later slots below it usually do not ask again.
- **A harness that dies at startup inside the sandbox**, often with "Read-only file system" for
  some path: run `"$H/worktrees/wrkslots" run smoke -- <harness> --version` from a Herdr pane to
  see the error, then add that path (for example a credential staging directory) to
  `sandbox.read_write` in `.wrkslots.yml`, with the owner's agreement.
- **The slot's own remote is local** (for example a test repository): boxed agents can only push
  to it if its path is in `sandbox.read_write`.
- **Removal refuses** while any process still uses the slot, and for a sandboxed agent until the
  slot's time-to-live has passed. Read the refusal; it names the condition. Never delete a slot
  by hand.

## Phase 7: Hand off to the owner

Tell the owner, briefly:

- what was installed and configured, and what was deferred (the chat bridge, if so);
- where the coordinator is: you start it in the step below, so they can switch to it in Herdr
  (workspace `<repo>-agents`, tab `coordinator`) or run `agentctl attach coordinator` from `$H`;
- which optional tools exist and that any of them can be added by asking ("set up tick-hub for an
  hourly status report");
- that the harness `AGENTS.md` "Project rules" section is theirs to fill, directly or by asking.

Then, with the owner's yes, **start the coordinator as a registered agentctl session**:

```sh
"$H/agent-utils/bin/agentctl" start coordinator --cwd "$H" --profile coordinator \
  --brief "You are the coordinator for this harness. Read AGENTS.md and the agent-utils-coordinator skill, then tell the owner you are ready."
```

Run it from `$H` so agentctl uses the harness registry. Registration is what makes the coordinator
addressable: tick-hub deliveries, other agents, and the owner's scripts reach it with
`agentctl send coordinator`, and `agentctl status coordinator` finds it. A coordinator the owner
starts by hand in some terminal is not registered, so nothing can deliver to it.

With chat configured, keep the coordinator registered: start it as above, then point the bridge at
it with `agentctl chat run`, using the pane and workspace identity `agentctl status coordinator`
reports (the agentctl chat guide's Setup section). `agentctl chat launch --config chat.json
--harness <harness>`, run from a Herdr shell tab in `$H`, is shorter, but the coordinator it starts
in that pane is not an agentctl session, so `agentctl send coordinator` (and so tick-hub delivery)
cannot reach it; if the owner prefers it anyway, say so and record it in the harness `AGENTS.md`.

If the owner instead asks you to "attach yourself to Herdr", follow the agentctl skill's section
of that name: a session that can be viewed remotely registers itself with `agentctl attach-self`
from `$H`; a local terminal session cannot be moved and hands off to the started coordinator.

Do not continue as the coordinator yourself: your session started in the checkout, which is now
`$H/$PRIMARY`. Once the coordinator is up, end this session. (If agentctl launched you, its record
names the old working directory, so `agentctl stop` refuses your pane; exit normally instead.)

## Add a tool later

Record every change in the harness `AGENTS.md` "Suite configuration" section, add the tool's skill
link to `.agents/skills/` if it has one, and commit the harness. For each tool, its own
`quickstart` and `--help` are the authority.

- **herdr-run**: from `$H`, `"$H/agent-utils/bin/herdr-run" init` writes an annotated
  `.herdr-run.yaml`. Its `allow` list is the owner's decision: propose it, do not widen it on
  your own. Offer a project-specific `workspace` label (for example `<repo>-cmds`) so the
  project's command tabs do not compete with other projects for the shared workspace's
  `max_panes` cap (`herdr-run status` shows the cap). Note in `AGENTS.md` which commands agents
  should run through it.
- **tick-hub**: tick-hub only evaluates reminders and prints `ACTION:`/`HEALTH:` lines; delivery
  to the coordinator is a small script. Start from `agent-utils/skills/agent-utils-setup/templates/
  tick-hub/`: copy `tick-deliver` to `$H/bin/`, `ops.yaml` to `$H/` (edit the reminders with the
  owner; `tick-hub quickstart` shows the format, and `tick-hub tick --config ops.yaml` previews a
  tick without changing state), and the `.service` and `.timer` files to
  `~/.config/systemd/user/<repo>-tick-hub.{service,timer}` with the placeholders filled (keep a
  tracked copy under `$H/systemd/`). Then `systemctl --user daemon-reload` and
  `systemctl --user enable --now <repo>-tick-hub.timer`. `tick-deliver` sends the tick's output to
  the registered `coordinator` session with `agentctl send`; a message that has to wait because
  the coordinator is busy stays queued and goes out on the next tick. Add `.tick-hub/` to the
  harness `.gitignore`.
- **dagrun**: it runs a DAG file owned by the primary repository (often `validation.dag.yaml`).
  Use `dagrun quickstart` to write one with the owner; it needs a user systemd manager and cgroup
  v2 for its resource limits.
- **wrkviz**: `wrkviz quickstart`; point it at the harness transcripts it lists.
- **gh-paced**: needs `cargo`. Build it from `agent-utils/rs` as its quickstart shows
  (`common/docs/gh-paced/QUICKSTART.md`), then put it in front of the real `gh` for agents.
- **vibe-talk**: a separately deployed web service; `agent-utils/vibe-talk/QUICKSTART.md` and
  its README are the reference. What a first run needs, in order:
  - a native build (`cargo build --release --locked` in `vibe-talk/`), installed to a versioned
    path rather than run from `target/`, unless the container image works on the host;
  - configuration and tokens outside any directory the owner syncs between machines;
  - `[storage] path` set, because the channel views the web app uses return 503 without it while
    `/healthz` still passes;
  - the built-in demo provider (`--fake-discord`) to reach a working page before wiring a real chat
    provider, and `[read_aloud] backend = "browser"` when no voice vendor is configured;
  - something in front that terminates TLS for the whole origin (HTTPS is required for the
    microphone and the installable app), listening on the address family the host is reachable
    on (some hosts are IPv6-only, where `0.0.0.0` is unreachable);
  - user services for the app and the TLS front end, enabled by the owner.
  Hand the owner `<URL>/voice` and the path of the token file, never the token itself; they paste
  the write token into the page once per browser. A host-specific guide, if the owner's
  environment has one, takes precedence over these generic steps.
- **pr-landing-planner**: advisory only; `pr-landing-planner quickstart`. Pair it with the
  `pr-landing-operations` skill only where the primary repository's rules authorize landings.
- **Turn the file-system sandbox on or off**: change `sandbox.isolation` in `.wrkslots.yml`, use
  the matching slot protocol in `AGENTS.md`, and smoke-test one slot (phase 6).
- **Update agent-utils**: `git -C "$H/agent-utils" pull --ff-only`, then re-read this skill's
  "What setup produces" for anything new, and update `{{AGENT_UTILS_COMMIT}}` in `AGENTS.md`.
