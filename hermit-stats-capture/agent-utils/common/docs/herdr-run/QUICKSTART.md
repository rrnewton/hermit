# herdr-run — quickstart

`herdr-run` runs an allowlisted command in a terminal pane belonging to a **separate terminal
server**, and hands back its real stdout, stderr, and exit code.

The pane is not a child of your process. Whatever constrains your process therefore does not
constrain the command: not its network policy, not its environment, not its lifetime. That is the
whole idea. Everything else here is about doing it safely, because the pane may also be somewhere
a human is typing.

This is the short version. `herdr-run userguide` is the complete reference.

## Four commands

```
herdr-run init                      # write an annotated .herdr-run.yaml in this directory
herdr-run status                    # what configuration, policy, and session are in effect
herdr-run check 'git status'        # would this be admitted? touches no pane
herdr-run run 'git status'          # run it, and exit with the command's own exit code
```

Only `run` touches a pane. `init`, `status`, and `check` change nothing at all, which makes them
safe to try first.

The command is **one quoted argument**. `herdr-run run git status` is refused rather than
re-joined, because re-joining loose words would silently change your quoting.

## The shape of the command line

```
herdr-run [GLOBAL OPTIONS] <subcommand> [OPTIONS]
```

Global options come **before** the subcommand and say who is asking and with what configuration:
`--config PATH`, `--agent NAME`, `--json`, `--version`.

Each subcommand's own options come **after** it. `run` takes `--dry-run`, `--cwd`, `--timeout`,
`--wait-ready`, and `--no-cache`. An option offered at the wrong level is refused with the level it
belongs to, so nothing has to be guessed at. `herdr-run <subcommand> --help` documents one
subcommand and nothing else.

```
herdr-run --agent release-agent run --timeout 60 'git push origin HEAD'
```

## Five things worth knowing before the first real command

1. **The allowlist is per project, and it is a human-only knob.** `herdr-run init` writes it with
   every setting present and commented. An agent that can widen its own allowlist does not have
   one, so keep that file somewhere the agents cannot write to.

2. **It is a cooperative rail, not containment.** The wrapper guarantees the pane executes the
   argument vector that was admitted and nothing else. It cannot guarantee anything about what an
   allowlisted program then does — `git` runs repository hooks, `gh` acts as you. "An agent may
   run git" is the privilege being granted.

3. **Exit codes pass through.** The process exits with the command's own status, so it composes in
   a script like the command it wraps. Wrapper failures use distinct codes: `77` refused, `75` pane
   busy, `69` session could not be established, `76` launched but still running, `78` configuration
   problem.

4. **The pane may be shared with a human.** By default a busy pane is refused immediately rather
   than typed over. `run --wait-ready S` waits instead. Tabs are recycled the same carefully: once
   the workspace holds `max_panes` panes, a new agent's tab replaces the least-recently-used tab
   whose shell is idle, and a tab running a command is never closed.

5. **`spool_dir` holds real command output and must be git-ignored.** It defaults to
   `.herdr-run/`.

## Checking one narrow scenario

If the reason you are here is that your own process cannot reach the network, `herdr-run
net-doctor` tests exactly that: it runs one probe directly and again through the pane and compares
them. It is a smoke test for that one situation and says so before it starts — not a health check
of the tool.
