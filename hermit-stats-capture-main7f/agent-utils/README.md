# agent-utils

A suite of command-line tools for running a team of coding agents on one
project: a coordinator agent that the owner talks to, subagents that each work
in their own isolated checkout, and the plumbing that keeps their work
visible, recoverable, and landed. Every tool also stands alone and has an
independently installable distribution.

**Website:** <https://rrnewton.github.io/agent-utils/> gives the overview (why the
suite exists, how the pieces fit together, and what each tool is for) and links
back into the documents here.

**To adopt the suite for a project**, open your coding agent in the project's
Git checkout and tell it:

> Adopt agent-utils for this project, following
> `skills/agent-utils-setup/SKILL.md` from https://github.com/rrnewton/agent-utils.

The agent walks you through a short guided setup: it detects what it can,
installs missing dependencies with your agreement, and asks only what it cannot
know. [QUICKSTART.md](QUICKSTART.md) shows a complete example on a one-commit
repository.

## The suite

```text
  owner ── chat (agentctl chat) ──┐      ┌── voice and phone (vibe-talk)
                                  ▼      ▼
                       ┌─────────────────────────────┐    timer ── tick-hub
                       │ coordinator agent           │◄── agentctl send ──┘
                       │ harness root, own Herdr tab │
                       └──────────────┬──────────────┘
                      agentctl start / send / read / stop
             ┌────────────────────────┼────────────────────────┐
             ▼                        ▼                        ▼
       subagent (Claude)       subagent (Codex)        subagent (Muse)      each a real TUI
       worktrees/slots/s1      worktrees/slots/s2      worktrees/slots/s3   in its own Herdr
             └─────── wrkslots slots: branches of the primary checkout ───────┘   tab
                                      │
             validate (dagrun)  ·  reach the network (herdr-run)  ·  pace gh (gh-paced)
                                      ▼
                             the project's remote
```

A project that adopts the suite gets a **harness**: a directory holding the
project's primary checkout, a `worktrees/` directory of agent slots, an
`agent-utils/` checkout, and an `AGENTS.md` that makes the agent started there
the coordinator. The coordinator follows the
[`agent-utils-coordinator`](skills/agent-utils-coordinator/SKILL.md) skill:
it turns the owner's goals into tasks, gives each task a fresh slot and a
subagent, checks what the subagents claim, lands what is good under the
project's own rules, and reports substance back to the owner.

| Layer | Tool | Status | What it does |
|---|---|---|---|
| Terminals | [Herdr](https://github.com/herdrdev/herdr) | external dependency | A persistent terminal server. Every agent runs as its native TUI in a Herdr tab, so a person can attach to any of them and take over. |
| Sessions | `agentctl` | core | Starts, messages, reads, and stops named agents across harnesses (Claude, Codex, Muse, and agentcloud workers), with durable prompt delivery and owner-controlled launch profiles. One interface for the owner and the coordinator alike. |
| Workspaces | `wrkslots` | core | Gives each agent its own Git worktree slot on its own branch, records ownership and handoffs, salvages unpushed work, and removes a slot only after proving nothing uses it. Each slot gets systemd resource limits, optionally lives in its own sparse disk image, and can be boxed into a file-system view where only the slot is writable. |
| Owner channel | `agentctl chat` | core | Bridges a chat space to the coordinator. Google Chat works today through the polling transport; other providers plug in through the subscription-plugin protocol, for which no provider ships yet. |
| Voice | `vibe-talk` | optional | A deployable service that gives chat-bridged sessions a phone and voice front end. |
| Recurring work | `tick-hub` | optional | One scheduled tick evaluates many recurring duties, each on its own cadence, and prints `ACTION:`/`HEALTH:` lines. A timer delivers them to the coordinator with `agentctl send`, the same queue the owner's messages use. |
| Sandbox escape | `herdr-run` | optional | Runs an allowlisted command, such as `git push`, in a visible Herdr pane outside whatever confines the agent, and keeps an audit record of every run. |
| Local CI | `dagrun` | optional | Runs a project's validation as a dependency graph under CPU, memory, and named-resource limits, with cgroup containment. `cpuset-alloc` and `parallel-experiment-runner` build on it. |
| Retrospective | `wrkviz` | optional | Builds a zoomable timeline of what the coordinator and its subagents did, from their transcripts, across agents and teams. |
| Budget awareness | `agent-usage` | optional | Reads each harness's plan usage and reset times (Claude Code and Codex, the numbers their `/status` screens show) without a model call, keeps a history, and reports burn rates over 15 minutes to 24 hours plus local token use. |
| GitHub hygiene | `gh-paced` | optional | Paces `gh` calls against per-account budgets so many agents sharing one GitHub account stay inside GitHub's limits and do not get the account suspended. |
| Landing | `pr-landing-planner` | experimental | Produces an advisory, conflict- and CI-aware plan for landing a queue of pull requests. |

The core tools are always set up; the optional ones are offered during setup
and can be added later by asking the coordinator. Each tool has a skill under
[`skills/`](skills/README.md), plus `quickstart`, `--help`, and `userguide`
commands that stay authoritative for its usage.

## Implementations

Paired tools share their core command contracts; installation-specific
extensions are identified explicitly below. The implementations are
intentionally independent. Shared fixtures, differential tests, isolated
package checks, and adversarial reviews catch schema, CLI, output, error, and
state-transition drift.

### Paired tools

| Command | Purpose | Python distribution | Rust crate |
|---|---|---|---|
| `dagrun` | Plan, visualize, and execute resource-aware CI DAGs with Linux cgroup containment and profiling. | `dagrun` | `dagrun` |
| `cpuset-alloc` | Reserve disjoint CPU sets and hard-pin benchmark process trees. | Companion command in `dagrun` | Companion binary in `dagrun` |
| `tick-hub` | Evaluate independently cadenced reminders and freshness checks in one deterministic tick. | `tick-hub` | `tick-hub` |
| `pr-landing-planner` | Produce advisory, conflict- and CI-aware pull-request landing plans. | `pr-landing-planner` | `pr-landing-planner` |
| `herdr-run` | Run an allowlisted command in a Herdr pane, outside whatever constrains the caller, with audited, byte-preserving results. An agent whose sandbox blocks the network is one such caller. | `herdr-run` | `herdr-run` |
| `agentctl` | Start named coding agents, delegate follow-up work, inspect goals, and retain direct terminal access. Interactive control requires Herdr. | `agentctl` (also worker, polling Chat, and MCP extensions) | `agentctl` (interactive core and event-driven plugin Chat service) |

Each distribution is independently installable and documented. Its README and
embedded user guide describe only that edition, so package-index users do not
need this source tree or knowledge of the sibling implementation.

### Python-only tools

| Command | Purpose | Python distribution |
|---|---|---|
| `wrkviz` | Build durable, zoomable local timelines from coordinator and subagent transcripts. | `wrkviz` |
| `parallel-experiment-runner` | Run boxed, resource-bounded concurrent seed sweeps through `dagrun`. | `parallel-experiment-runner` |
| `wrkslots` | Provision, box, hand off, and safely reclaim isolated Git worktree slots, one per agent. | `wrkslots` |
| `agentctl` extensions | Headless workers in Herdr or tmux, the legacy polling Chat transport and launcher, and an MCP interface over the same sessions. | Included in `agentctl` |

These tools are independently installable and follow the same package
documentation and artifact checks. They are explicit exceptions to the
two-language implementation and behavioral-differential contract.

`gh-paced` and `agent-usage` are Rust-only tools. They are built from
`rs/gh-paced` and `rs/agent-usage` and ship in no package index; see the
[gh-paced quickstart](common/docs/gh-paced/QUICKSTART.md) and the
[agent-usage quickstart](common/docs/agent-usage/QUICKSTART.md).

## Persistent coding agents

`agentctl` gives a person or coordinator one interface for long-lived workers:

```sh
cargo install --path rs/agentctl
agentctl quickstart
agentctl capabilities
agentctl chat quickstart
```

The Python package includes interactive and headless workers, the polling Chat
transport and launcher, and MCP. The Rust crate supplies interactive control and
a durable event-driven Chat host for installed subscription plugins and an
operator-selected outbound helper. Both provide `agentctl userguide` and
per-command help; `capabilities` reports the installed adapters. Interactive
control and either Chat bridge require Herdr. Headless workers can use Herdr or
tmux for their transcript view.

`agentctl` has no dependency on the `herdr-run` shell executor. Both call Herdr
through their own adapters. The compatibility names `herdr-agent`,
`herdr-subagents`, and `herdr-chat` belong to the Python agent-control package.
Start new integrations with `agentctl`.
See [public related work](common/docs/agentctl/RELATED_WORK.md) for comparisons
with other session-control and chat approaches.

## Repository layout

```text
common/docs/       settled documentation, public related work, and rendered editions
ai_docs/transient/ dated working designs and temporary research
cross/             behavioral differential harnesses and shared fixtures
examples/          runnable DAG examples
vibe-talk/         a deployable service, outside the workspaces (see below)
py/                independently publishable Python distributions
rs/                independently publishable Rust crates
scripts/           documentation, package, and dependency contract checks
site/              the public website: hand-written HTML, checked by scripts/check_site.py
skills/            agent-facing skills: suite setup, the coordinator, and one per tool
```

Several tools use a shared documentation renderer that combines:

```text
common/docs/<tool>/README.template.md
common/docs/<tool>/USER_GUIDE.template.md
common/docs/<tool>/fragments/{python,rust}/{README,USER_GUIDE}.md
```

It writes authoritative rendered editions under `common/docs/`; package trees
link to those files. `agentctl` instead keeps its CLI documentation assets in
`py/agentctl/`, with the core guide shared by both implementations. Package builders dereference the links into ordinary files,
so every installed artifact is self-contained. Check mode verifies the exact
rendered content, link topology, and absence of sibling-language, source-tree,
unrelated-project, or development-history references:

```sh
python3 scripts/embed_userguides.py
python3 scripts/embed_userguides.py --check
```

## Development

Build both editions:

```sh
make both
```

Run the repository contract:

```sh
python3 scripts/embed_userguides.py --check
cargo fmt --all --manifest-path rs/Cargo.toml -- --check
make both
make check
make test
python3 -m mypy cross/differential.py
python3 cross/differential.py --tool dagrun
make cross
make check-packages
```

The differential harness runs matching commands over valid, invalid, boundary,
and randomized inputs. Human-oriented help may use idiomatic wording, while
machine schemas, normalized results, exit behavior, and state transitions are
cross-checked as part of the contract. A check that cannot run on the current
host is reported as skipped, is never counted as a pass, and fails these
full-coverage commands; see
[skipped checks and partial runs](cross/README.md#skipped-checks-and-partial-runs).
Independent findings and reproducible evidence are recorded under
[`reviews/`](reviews/README.md).

## Services

`vibe-talk/` is a **service**, not a command-line tool: a Rust web server that bridges a voice agent
to Discord channels, deployed as a container rather than installed from a package index. It is
therefore an explicit exception to the two-language, two-package contract above — it has one
implementation, its own Cargo workspace and lockfile, and its own CI workflow rather than a place in
`make check` / `make test`, so its web-server dependency tree cannot perturb the published tools'
MSRV or lockfile. See [`vibe-talk/README.md`](vibe-talk/README.md).

## Package documentation

- [Python distributions](py/README.md)
- [Rust crates](rs/README.md)
- [Adversarial review evidence](reviews/README.md)

## License

MIT — see [LICENSE](LICENSE).
