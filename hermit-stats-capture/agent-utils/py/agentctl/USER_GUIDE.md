# agentctl user guide

`agentctl` lets a person or coordinator agent manage persistent coding sessions:
start them, submit follow-up work, inspect results, and take over their terminals.
The sessions keep their own native tools, instructions, and conversation. This
is useful when one coordinator needs several long-lived workers that a human
can still examine and steer directly.

## Dependencies and available adapters

The core interactive adapter calls Herdr's public CLI directly. Install Herdr
and the selected Codex, Claude, or Muse harness separately, and authenticate the
harness before launching workers. The integration uses Herdr's pane, tab,
workspace, agent prompt, agent wait, and session inspection APIs.

The manager itself is a short-lived command. Herdr owns an interactive agent's
terminal process. There is no manager daemon or hosted coordination service to
keep alive. The manager is independent of shell-command execution utilities.

Run `agentctl capabilities` to inspect the modes, backends, harnesses, and
services present in this installation. **The Python distribution includes the
interactive core, headless workers, a polling Chat transport and launcher, and
MCP. The Rust distribution provides the interactive core plus a durable
event-driven Chat host, a provider-neutral inbound-subscription protocol, and
safe plugin discovery.** It does not ship a provider executable: an operator
installs an inbound plugin and separately selects any outbound helper. Headless
workers and MCP remain bundled capabilities of the Python distribution.

`list` and `status NAME` retain the launch profile name, requested model and
reasoning effort, starting directory, and any slot, project and isolation
settings even when Herdr is unavailable. `native_session` records the native
conversation separately from the observed pane identity. New Claude launches
receive a generated UUID through `--session-id`; resumed launches retain the
explicit conversation. Codex and Muse record a conversation when their harness
reports it. Agentcloud retains its existing session ID. An unreported ID stays
unknown; agentctl does not guess from nearby transcripts or the current directory.
Environment metadata contains names only; values remain in private profiles.

| Mode or service | What it controls | Additional dependency |
| --- | --- | --- |
| Interactive | A native Codex, Claude, or Muse TUI that remains directly accessible | Herdr |
| Headless worker extension | Resumable Codex, AGY, or Muse structured turns; a terminal shows the transcript | Herdr or tmux and the chosen harness |
| Chat service | Provider messages connected to an interactive coordinator | Herdr and either the Python Google Chat transport or a Rust subscription plugin |
| MCP extension | The same session operations over local MCP stdio | An MCP client |

### Rust subscription plugin discovery

The Rust command inspects `~/.agentctl/plugins/` when `agentctl capabilities`,
`chat init`, or `chat run` needs plugin inventory. Ordinary session-control
commands do not scan the plugin directory. This installation provides the
protocol, safe discovery, process supervision, and durable subscription host;
it does not provide a provider executable. Set `AGENTCTL_HOME` to an absolute
directory to use an isolated user-level home.
This home is separate from the project-local session registry selected by
`--registry`; installing a backend cannot change or acquire a project session.
Manifests are discovered automatically in deterministic filename order; there
is no registration command and no project-local plugin registry to update.

Discovery is manifest-only and never executes every file in a directory. One
plugin occupies `plugins/NAME/`, with a private `manifest.json` and the contained
direct-child executable named by that manifest:

See the [normative version 1 process
protocol](https://docs.rs/chat-subscription-plugin/0.1.0/chat_subscription_plugin/) for exact frame
shapes, ordering, durable Commit semantics, and canonical bounds.

```json
{
  "schema": "agentctl-plugin-manifest/v1alpha1",
  "name": "example-chat",
  "capability": "chat-subscription.example",
  "executable": "backend",
  "protocol": {"name": "agentctl-chat-subscription", "min": 1, "max": 1},
  "implementation": "implemented"
}
```

`process_phase_timeouts` is an optional manifest object for a plugin whose
bounded provider operations cannot use the default host deadlines. When present,
all five members are required and their units are seconds:

```json
{
  "process_phase_timeouts": {
    "hello_seconds": 20,
    "start_seconds": 130,
    "commit_seconds": 45,
    "close_seconds": 57,
    "shutdown_grace_seconds": 0
  }
}
```

Omitting the object preserves the exact default Hello, Start, Commit, Close, and
shutdown-grace values of 10, 30, 30, 10, and 2 seconds. Required protocol phases
must be positive and are capped at 30, 180, 60, and 120 seconds respectively;
shutdown grace may be zero and is capped at 10 seconds. Discovery refuses an
incomplete object, an unknown member, or a value outside those bounds before it
spawns the executable.

The home, plugin directory, manifest, and executable must be owned by the
current account and must not be group/world writable. Symlinks, hard-linked
files, path traversal, oversized or changing manifests, duplicate capability
identities, and incompatible protocol ranges are refused. `agentctl
capabilities` lists safe compatible entries under `discovered` and rejected
entries under `refused`; it distinguishes these from the built-in subscription
core.

Installers should assemble and validate a complete private staging directory on
the same filesystem, with real single-link `manifest.json` and executable
files, then atomically rename it into `plugins/NAME/`. Upgrades may use an
atomic directory exchange before retiring the old directory. A versioned store
may live outside `plugins/`, but activation must copy real files rather than
symlink or hard-link them into the discovery tree. This keeps discovery from
observing a partial install. Launch reopens every path through pinned directory
descriptors, requires the exact executable identity discovered earlier, and
executes through that pinned descriptor; exchanging the activation directory
before or after command construction cannot redirect execution. Provider
plugins are native executables rather than interpreter scripts, because the
pinned executable descriptor is close-on-exec.

Capability identities name both provider and implementation so alternatives
can coexist, for example `chat-subscription.google-chat.workspace-events` and
`chat-subscription.google-chat.polling`. Reusing the exact capability identity
is a collision: every colliding entry is refused rather than selecting a
filename-order winner.

When a subscription host launches an accepted plugin, it clears the inherited
environment first. Only a small non-secret process baseline (`HOME`, `USER`,
`LOGNAME`, `PATH`, locale variables, `TMPDIR`, and `XDG_RUNTIME_DIR`) is copied.
The manifest cannot request environment names, arguments, or credentials and
cannot self-authorize access to unrelated API keys. Provider credentials belong
to an explicit host-controlled mechanism or to the provider's ambient user
credential service.

The Rust `chat run` host connects the returned process supervisor with explicit
Hello, Start, Commit, Close, and shutdown-grace deadlines. Blocking protocol I/O
runs on an owned worker. Before Hello begins, the host publishes the selected
generation's complete shutdown budget. After Hello, it installs the separately
retained cancellation handle before Start begins, so a blocked Start is
interruptible and its full independent phase deadline is not additive during
shutdown. The handle serializes Close while `next_item` is blocked. Success
requires that exact read to observe post-Close EOF, semantic backend cleanup to
finish, and the pidfd-owned child to exit zero. The configured Close deadline is
followed by the chosen process-exit grace and only then forced process-group
termination. Leader exit uses event-driven Linux pidfd notification rather than
periodic process polling, with a fixed two-second post-kill bound. Launch
preflights both atomic clone3 pidfd creation
and `waitid(P_PIDFD)`, and the non-execing process-group supervisor closes all
inherited host descriptors through `close_range` or `/proc/self/fd` before it
waits. The launch thread masks the complete kernel signal set only across
`clone3` and restores its exact prior mask; the sentinel permanently retains
that mask so inherited handlers and blockable group signals cannot terminate
it. A parent-death signal and one-byte release handshake keep the raw sentinel
from surviving a fatal parent mask-restoration failure before tracked pidfd
ownership is established. Reaping uses only the pidfd identity, never a
numeric-PID wait fallback. A host without those supervision primitives refuses
before spawning the plugin executable. Cleanup ownership is globally capped at
32 admissions, so a stuck reap or join makes later launches fail closed instead
of growing threads or processes without bound. Unexpected pidfd-reap errors
retain that admission and tracked cleanup thread until an explicit later retry
succeeds. If the plugin
pidfd transfer fails, the host kills the pinned group and permanently retains the
identity resources plus one admission instead of using numeric `Child::kill` or
`Child::wait`. Launch also requires the default `SIGCHLD` disposition because an
auto-reaper makes status ownership unsafe. The process
group is a cleanup boundary for trusted same-user plugins, not security
containment—a malicious plugin can escape it with `setsid`. Untrusted plugins
need a launcher-owned sandbox or cgroup. The Rust CLI exposes this runtime as
`chat init`, `chat status`, `chat inspect`, `chat tick`, `chat run`, and `chat close`; use
`chat quickstart` and `chat userguide` for its configuration and operations.

Derive the service-manager stop interval from the manifest selected by that
service; 75 seconds is not a protocol-wide value. The Rust host's worst-case
provider window before Hello completes is
`hello_seconds + close_seconds + shutdown_grace_seconds + 7`; after Hello it is
`close_seconds + shutdown_grace_seconds + 7`. An admitted reaction or reply
helper independently owns
`outbound_command.timeout_millis + outbound_command.shutdown_grace_millis +
7000` milliseconds. The hard service window is the maximum of the applicable
provider window and that outbound window, plus measured service-manager
scheduling margin. The final seven seconds reserve two seconds for forced pidfd
reap and five seconds for host reconciliation and worker joins. At startup the
service pins the complete selected provider timeout tuple; a later generation
whose manifest differs is rejected and requires a service restart, so the
configured outer bound cannot silently become stale. The accepted manifest
maxima require up to 167 seconds during Hello and 137 seconds afterward; the
maximum accepted outbound window is 67 seconds. A deployment-local 75-second
setting with `close_seconds=60` and `shutdown_grace_seconds=2` is safe only if
its selected `hello_seconds` is at most 6; the default 10-second Hello requires
at least 79 seconds before additional service-manager scheduling margin.

With systemd `KillMode=control-group`, use the same binary's hidden
`chat graceful-stop-main --main-pid "$MAINPID" --main-pidfd-id "$MAINPIDFDID"`
as synchronous `ExecStop`. It opens a fresh pidfd, requires its inode to equal
systemd's `MAINPIDFDID`, signals only that stable identity, and waits on the
pidfd. `MAINPIDFDID` in service-manager commands requires systemd 258 or newer.
Pair the helper with the derived `TimeoutStopSec`, plus
`TimeoutStopFailureMode=kill`. After its 70-second diagnostic threshold the
helper deliberately remains blocked: returning would start a second systemd
stop interval instead of letting the original window kill the complete control
group.

A manifest can report only static implementation maturity. Discovery always
reports `configured`, `connected`, and `live_verified` as false. Those are
runtime facts requiring separate evidence; installing a manifest cannot claim a
live provider connection. The reference inventory reports Google Workspace
Events design material with no Rust provider, cloud topic, pull
subscription, application-default credentials, or live end-to-end verification.
It reports Discord request/response support separately from the absent Gateway
subscription. Those reference entries are design inventory, not working inbound
backends; installed plugin manifests appear separately.

Extension commands are not available in every installation. An interactive
agent requires Herdr; selecting tmux does not provide interactive agent control.
A headless transcript view is not a native interactive harness interface.

## Names, state, and shared workspaces

Commands use a human-readable session `NAME`, made of lowercase letters, digits,
and hyphens. State defaults to `.agentctl` in the current directory. Use
`--registry /absolute/path` to give multiple callers the same registry. The
`--state` spelling is an alias for the session registry. Chat has a separate
state directory: the polling edition names it with `--state`, while the
subscription-plugin edition uses the unambiguous `--bridge-state`.

A name identifies one generation of a session. The registry also records its
runtime identity, selected adapter, native conversation when known, and delivery
artifacts. Reusing a name requires stopping its current session first. The tool
refuses a changed or unverified owner rather than treating a matching directory
or pane title as proof that it found the same agent.

Workers run in the directory given by `--cwd` (default: the current directory).
They share its files and credentials. Separate Git worktrees are an operator
choice when simultaneous edits need isolation; agent control does not require
creating worktrees.

## Private launch profiles and harness skills

Repeated harness policy can live in `.agentctl/profiles.json` under the working
directory. When `--cwd` has no profile file, `start --profile` and `profiles`
use the one beside the registry, when the registry directory is named
`.agentctl`: an agent started in a worktree outside the project (for example a
slot directory) still uses the project's profiles. The file is deliberately
project-local and untracked. `agentctl`
requires `.agentctl/` to match a Git ignore rule, requires the directory and
file to be owned by the current user with no group or other permissions, and
refuses symlinks, hard links, unknown fields, malformed text, NUL, and
credential-shaped environment or argument names.

```json
{
  "schema": "agentctl-profiles/v1",
  "workspace": "project-agents",
  "profiles": {
    "preferred-reviewer": {
      "harness": "muse",
      "mode": "interactive",
      "model": "provider-model-id",
      "reasoning_effort": "high",
      "argv": [],
      "env": {"ROUTING_SELECTOR": "provider-route"}
    }
  }
}
```

The top-level `workspace` is optional. When present, it is the required Herdr
workspace label for every interactive session in the project. Starts resolve or
create that label, ignore an ambient `HERDR_WORKSPACE_ID`, and refuse an explicit
workspace ID that does not resolve to it. Automated input and reads refuse a pane
in any other workspace. Status, attach, goal queries, and wait remain available
for diagnosis; stop remains available for safe retirement unless a durable move
is incomplete. When the key is absent, starts use `HERDR_WORKSPACE_ID` when set,
otherwise the shared `subagents` workspace.
The label must be 1–128 UTF-8 bytes, have no surrounding whitespace, and contain
no control characters.

Within each profile, `harness` and `mode` are required. `model`,
`reasoning_effort`, `argv`, and `env` are optional. Values are parsed as JSON strings and passed as literal
arguments or environment entries; they are never shell-expanded. A profile
cannot specify the model or effort both structurally and in `argv`. When
`--profile NAME` is present, explicit `--harness`, `--mode`, `--model`,
`--reasoning-effort`, `--harness-arg`, and `--env` are refused instead of
silently winning or losing. The config is not a credential store. Environment
values are not written to session records or profile-list output; raw arguments
are launch policy and therefore must not contain secrets. Raw Codex
`-c`/`--config` and `-p`/`--profile` arguments cannot accompany a structured
model or effort because those opaque settings could override the structured
selection.

Interactive profiles support Codex, Claude, and Muse, plus `agentcloud` in the
Rust distribution (see [Agentcloud sessions](#agentcloud-sessions)). Headless profiles support
Codex, AGY, and Muse in installations that include the worker extension.
Headless Muse keeps `exec`, `--json`, `--session-id`, and the prompt under the
runner's control. Extra options with values use one literal `--option=value`
entry; repeatable options such as `--image=...` may be repeated.

```sh
chmod 700 .agentctl
chmod 600 .agentctl/profiles.json
agentctl profiles --cwd .
agentctl start reviewer --cwd . --profile preferred-reviewer
```

To resume a saved local interactive Claude, Codex, or Muse conversation with
a project profile's launch settings, combine `--profile` with `--resume`:

```sh
agentctl start reviewer-again --cwd . --profile preferred-reviewer --resume SESSION
```

The profile still selects the harness, model, effort, arguments, and environment.
Its raw arguments cannot repeat the resume selector. A conversation already
registered under another agent name is refused. Headless and agentcloud profiles
do not accept `--resume`.

`agentctl skill install` installs the bundled `agentctl` skill for Codex,
Claude, and Muse. A byte-identical installation is left unchanged. Divergent
content and symlink destinations are refused unless a regular file is replaced
with explicit `--force`; symlinks are never followed. Select one or more targets
with repeatable `--harness`. Codex and Claude receive the file directly in
their configured skill roots. Muse is installed through its native
`muse skills install --scope user` command and managed XDG skill store; the
command is skipped when that managed copy is already byte-identical.

## Start and delegate

```sh
agentctl start reviewer --harness codex --cwd /work/project \
  --brief 'Review the current changes and report actionable findings'
agentctl start implementer --harness claude --cwd /work/project \
  --file /tmp/implementation-task.txt
agentctl start gateway-worker --harness codex --cwd /work/project \
  --env ROUTING_SELECTOR=provider-route \
  --brief 'Use the configured gateway for this task'
agentctl list
agentctl status reviewer
agentctl send reviewer 'Focus on cancellation and restart behavior'
```

`--model` selects an accessible model; omission preserves the harness default.
`--reasoning-effort` maps to the selected harness's structured effort option.
`--harness-arg=ARG` passes a literal argument to an interactive harness and may
be repeated. `--resume SESSION` resumes an explicitly identified conversation.
When `--resume` is set, raw Codex `resume` and Claude
`--resume`/`--continue` selectors are refused before registry state is created.
Use `--workspace-id` to choose an exact Herdr workspace. Its default is the
configured project workspace, then `HERDR_WORKSPACE_ID`, then the shared
`subagents` workspace. When project config contains `workspace`, an explicit ID
must resolve to the same label. Adoption's `--workspace` assertion must also
equal that configured label. To migrate an existing agentctl-owned native Herdr agent without
restarting its process or native conversation, run:

```sh
agentctl move reviewer
```

The configured destination workspace must already exist. Move accepts only a
running owned native Herdr agent. It verifies the source pane, one-pane tab,
agent name, harness, working directory, destination label, and Herdr's returned replacement IDs.
It then rebinds the durable delivery queue and commits the new pane/tab/workspace
identity. Adopted and custom-pane sessions are refused because they do not have
the native name-based recovery identity required after a cross-workspace move.
If the process is interrupted after Herdr moves the pane, status reports the
pending destination and instructs the operator to rerun `agentctl move NAME`.
That repeat recovers and commits the new identity. Stop refuses while such a
move intent is pending so it cannot archive the old record and orphan the moved
pane. If project configuration no longer contains `workspace`, the repeat uses
the exact destination workspace ID already stored in the intent; it never
retargets the move from ambient state. A repeat after the record was committed
but before intent cleanup recognizes the completed destination and removes the
stale intent.

Interactive Herdr starts also accept repeatable `--env KEY=VALUE`. Each entry is
passed as one literal argument to Herdr when it creates the tab, before the
harness starts; spaces, shell characters, additional `=` characters, Unicode,
and an empty value are not expanded or rewritten. Names must match
`[A-Za-z_][A-Za-z0-9_]*`, and neither names nor values may contain NUL. This
option does not apply to headless workers.

Herdr 0.9.3 can report Codex as `unknown` while its composer is idle. For
startup and durable drain, agentctl can prove readiness from an empty Codex
composer with a recognized idle footer, checking the same foreground process,
terminal, and native identity before and after reading the screen. Busy input,
drafts, menus, and trust prompts still block delivery. The reported status
remains `unknown`. A startup timeout is accepted only for Herdr's exact startup
timeout response and this positive readiness proof; other launch errors remain
failures.

Muse is launched through Herdr's generic `pane run` interface. `agentctl`
resolves the installed executable, launches a shell-quoted literal argv,
verifies its kernel `/proc/PID/exe` identity and any supplied foreground argv
in the allocated pane, and then publishes pane-local agent
status. New owned sessions persist the Linux boot ID, PID, process start time,
and executable device/inode before readiness checks. Subsequent operations bind
to that tuple and intentionally do not consult the installation pathname, so
an atomic package upgrade may leave `/proc/PID/exe` marked `(deleted)` without
invalidating the still-running session; the new image applies only to future
sessions. Interactive shebang scripts and other binfmt wrappers are refused
because their kernel executable identity belongs to the interpreter rather
than the configured harness. Records without the tuple retain strict
current-path verification and are not auto-migrated after their image is
already deleted. It never resolves this adapter with `agent get NAME`. A visible
workspace-trust prompt fails startup and leaves the pane for a human; only an
owner-configured literal `--trust-workspace` argument bypasses that refusal.
Herdr may omit foreground argv. A new launch can still be pinned only when its
kernel executable matches the already-opened launch image; names and command
text cannot substitute for that proof. Malformed supplied argv is refused.
For delivery, the adapter verifies the same foreground Muse process and a bare
idle composer, inserts the prompt as one bracketed literal paste with Herdr's
`pane send-text` (so embedded newlines cannot submit it), proves the draft is
visibly present in the bottom composer, re-verifies the recorded process, and
only then sends `Enter`. It marks
the queue item delivered only after the same literal prompt has moved out of
the composer into Muse's transcript. A redraw or error that leaves the draft in
the composer is not acceptance. Failure
after either input boundary remains `possibly_submitted`; it is never replayed
automatically.
Requested reasoning effort is launch intent, not proof of the provider's
resolved effort. In deployments where an `ultra` gate is closed, Muse can
visibly resolve the request to `xhigh`. `start` and `status` preserve that
bounded sentence as `startup_warning` and report the explicitly selected
`effective_reasoning_effort`; they do not relabel requested `ultra` as the
effective setting. The live footer remains the primary UI evidence.

Environment values are launch-only input. `agentctl` does not write them to the
session registry or return them from `status` or `list`. They still become part
of the launched process environment and are not a credential vault; use the
host's ordinary credential facilities for secrets. If a launch using `--env`
fails, its retained status record uses a generic diagnostic so terminal-control
errors cannot copy a value into later status output; the immediate command still
reports the launch failure to its caller.

### Revive a dead owned harness

After a harness is killed or its terminal server restarts, its registry record
can still say `running`. Inspect the recovery plan, then resume its recorded
native conversation in a fresh tab:

```sh
agentctl revive reviewer --dry-run
agentctl revive reviewer
agentctl revive --all --dry-run
agentctl revive --all
```

Revive requires proof that the recorded harness process is dead and that its
old presentation is either absent or the same idle shell in a one-pane tab.
A live process, a server outage, changed pane ownership, or an unknown native
conversation blocks recovery. Adopted agents, headless workers, and remote
agentcloud sessions are not eligible. `--expected-token TOKEN` binds a named
revive to the old generation; it does not apply to `--all`. The replacement
startup deadline is `--startup-timeout SECONDS`, greater than zero and at most
300 seconds, default 30.

The recorded model, reasoning effort, literal policy arguments, cwd, pause
state, and slot isolation are retained. A recorded profile must still match
those launch settings. Environment values come from that private profile;
records store only their names. A changed or missing profile, an unrecoverable
literal environment, or a changed slot mapping blocks revival before launch.

Once the replacement is verified, the complete old generation is archived,
including its queue, quarantine, and terminal output. The stale pane and its
one-pane tab are closed after publication. Old queued prompts and goal delivery
receipts are never replayed into the replacement; the recorded goal text remains
available for the coordinator to inspect and assign deliberately.

An interrupted revive reserves the name and both generations. Rerun the same
command to finish publication or stale-tab cleanup; it never starts a second
replacement. If a launch was interrupted before usable process and terminal
anchors were saved, recovery remains blocked and identifies the retained
candidate record and terminal for inspection. `--dry-run` does not change
records, locks, tabs, or processes. Batch recovery skips live and non-running
records, continues past blocked names, and exits 75 if any name remains blocked.

### Adopt an existing Herdr agent

An agent that is already running in Herdr can join the same named registry
without being restarted or renamed:

```sh
herdr pane list
herdr pane get w1:p2
herdr workspace get w1
agentctl adopt reviewer --pane w1:p2 --workspace project \
  --cwd /work/project --harness codex
```

Muse can be adopted with `--harness muse` when Herdr reports that exact live
harness. Adoption requires the foreground process and terminal to be pinned,
along with the recorded shell generation and the identity assertions below.
Delivery uses the same verified composer, process checks, and Auto-review
policy as an owned Muse session. Adoption does not report a new agent, change
its label, or take ownership of its runtime.

Use the pane ID, working directory, harness, workspace ID, and workspace label
reported by those read-only Herdr commands; do not infer them from a tab title.
`--pane`, `--workspace`, `--cwd`, and `--harness` are required identity
assertions. Adoption resolves the workspace label, canonicalizes the directory,
requires a live matching harness, pins the exact pane and tab, and refuses a
pane or native session already registered under another name. A pane title or
tab label is never enough. If Herdr already reports a native conversation ID,
the record and durable queue retain it automatically. `--session ID` may be
supplied only to assert an ID already reported by that exact live pane; it is not
a way to guess or overwrite native identity.

If `pane get` reports no native session, omit `--session`; the adopted queue
stays bound to the exact pane. A separately recovered ID can later enable native
goal inspection with `agentctl bind-session NAME ID`. That explicit fallback
does not rewrite the queue target and is refused if another registered record or
live pane already reports the same harness-local ID.

The saved adapter is `herdr-foreign`. Named `send`, `read`, `wait`, `goal`,
`bind-session`, `status`, `list`, pause/resume, attach, and durable drain then use
the same interface as a started interactive session. The registry owns those
control artifacts, not the adopted process. Consequently, `agentctl stop NAME`
revalidates the target, saves a final terminal snapshot, unregisters it, and
archives its queue without closing the pane, tab, or process. Stop the foreign
runtime through the authority that created it. Adoption accepts Herdr's pane
shell only when kernel process inspection identifies a supported shell image,
then records its boot ID, PID, process start ticks, and executable device/inode.
Normal `stop` requires that exact shell generation both before and after the
snapshot, including while the foreign agent is still live. If Herdr no longer
reports the agent or its native session, `stop` additionally proves the same
recorded pane and tab contain that exact idle shell generation. An adopted
record without the shell identity refuses automatic retirement. A still-live
agent may move between tabs inside the recorded workspace because its pane,
harness, working directory, native session (when reported), and original shell
generation remain independently pinned. Any other missing or changed identity
refuses and leaves the registration in place.

The supported-shell role is selected from the kernel-reported executable
basename (`bash`, `zsh`, `sh`, `dash`, `fish`, or `ksh`) and then pinned by
boot/process/image identity. It is not a package-signature or trusted-binary
attestation; another executable installed under one of those basenames is
outside this same-user cleanup boundary.

### Retire a dead adopted record after a restart

After a Herdr restart, an adopted record can refer to a shell generation that
has ended. A stop refusal prints a recovery command when agentctl can prove
that the record's saved harness generation is dead:

```sh
agentctl stop reviewer --retire-dead-adoption --expected-token TOKEN \
  --expected-record-sha256 SHA256
```

Use the exact token, raw record digest and registry from the printed command.
The selector requires a running adopted Herdr record with a stored harness
process identity, and both assertions are mandatory. A live or unverified
generation, a changed record, or an incomplete identity transaction blocks
retirement. The digest is the lowercase 64-hex SHA-256 of the exact
`agent.json` bytes, including formatting and unknown fields.

This recovery archives the original record, saved output and entire queue,
including quarantine, without querying Herdr or changing any pane, tab or
process. It works while Herdr is offline and leaves the original archived
record bytes intact. It is separate from the explicit recovery for records
without a saved shell identity; their selector and proof requirements are
listed by `agentctl stop --help`.

`send` accepts literal text or `--file`. In interactive mode, the instruction is
persisted before submission. The manager waits for native readiness, records an
in-flight barrier, and asks Herdr to submit the text with paste plus Enter. It
requires evidence of a subsequent working state to confirm acceptance.

The default readiness wait is 900 seconds. For a responsive coordinator loop:

```sh
agentctl send reviewer --file /tmp/follow-up.txt --ready-timeout 0
agentctl drain reviewer --ready-timeout 0
```

`agentctl wait NAME` returns when the agent is ready for input again, which for
an interactive agent means its current turn has ended. Read the result with
`agentctl read`; readiness says nothing about whether a longer goal is done.
Some harnesses confirm a prompt by the screen alone: the prompt has left the
composer before the agent visibly starts working. So for 10 seconds after the
newest confirmed delivery, `wait` does not accept an idle state until it has
seen the agent busy. A `wait` started right after `send` therefore waits for
that turn instead of returning at once, and a short timeout in that window
reports that the agent has not been seen working yet.

Interactive timeout arguments accept finite seconds no greater than 31,536,000
(one year). Readiness waits may be zero; working confirmation must be positive.
The startup deadline must be positive and at most 300 seconds. The count options
`--max-attempts` and `--lines` must be between 1 and 1,000,000.

Known-unsubmitted work remains queued. Uncertain submission is quarantined and
is not automatically injected again. `--message-id` gives an interactive
request a caller-selected identity; duplicate IDs are rejected, not interpreted
as permission to execute again. Inspect the saved artifact before deciding
whether a new instruction is needed.

The interactive queue has four durable directories:

| Directory | Meaning | Recovery |
| --- | --- | --- |
| `inbox/` | The request is safely pending and has not crossed the submission barrier | Run `agentctl drain NAME` against the same registry and queue |
| `inflight/` | Submission may have begun; a crash here leaves acceptance uncertain | Inspect the harness; restart quarantines it without replay |
| `failed/` | Quarantined input, with an error record distinguishing invalid input from `possibly_submitted` | Inspect the error and conversation; never automatically replay uncertain work |
| `processed/` | The target's working transition confirmed submission | Await the agent's result; this is not proof of task completion |

`drain` types the prompts in `inbox/` one at a time, oldest first: by the
`queued_at` time recorded when each was queued, then by file name for equal
times. An entry whose `queued_at` is missing or is not a finite number goes
before every timed entry, by file name. `drain` stops at the first prompt that
has to wait, for example because the pane is busy, and leaves that prompt and
every later one in `inbox/`, so a waiting prompt keeps its place.

`send` reports `outcome: pending` with exit 75 for safely queued work and
`outcome: possibly_submitted` with exit 76 for uncertain delivery, including the
request ID and artifact path. Preserve that ID and use `drain` for pending
work. Sending the same task under a new ID creates a second request and can
duplicate work; it is not a recovery operation.

## Boxing an agent to a wrkslots slot

```sh
agentctl start implementer --harness codex --cwd /work/project --slot slot07 \
  --brief 'Implement the parser change in this slot'
```

`--slot SLOT` (interactive Herdr sessions only) runs the agent inside a wrkslots
slot's box (see the wrkslots user guide). agentctl asks
`wrkslots shell-command SLOT --format json` for an exec-only command line (from
`AGENTCTL_WRKSLOTS_BIN`, else `wrkslots` on `PATH`, run in `--slot-project DIR`
or `--cwd`), runs it in the new pane, and waits until the pane's own shell PID
is once more the sole foreground process, is a shell, and sits in a wrkslots
slice. Only then does it start the harness. The agent's recorded working
directory is the slot directory. The box is the same whether the slot is plain
worktrees or a disk image: per-slot limits, and a file-system view in which
only the slot, its Git directories, the project's blessed outputs, and the
shared harness state are writable.

`--slot-isolation userns|cgroup|root` overrides the project's
`configuration.sandbox.isolation` for this agent: `userns` builds the view in an
unprivileged user namespace, `root` builds the same view through a short-lived
`sudo -n` launcher (for harness launchers that perform a setuid step, which a
user namespace refuses), and `cgroup` applies limits only.

Under `root` isolation sudo stays the pane's foreground process and runs the
boxed program on a private terminal, so Herdr can neither start nor detect a
harness there. agentctl therefore runs the boxed harness line in the new pane
itself (adapter `herdr-relay`, Claude and Codex only). It pins the program behind
the relay by kernel identity (the first process below the pane's shell that is
yours and shares the slot's cgroup scope) and reports the pane to Herdr as that
harness. It reads the harness state from Herdr's live screen rules
(`herdr agent explain`) plus the harness's interrupt hint, and types prompts
through the same screen-verified submission it uses for native Claude and Codex
panes. Herdr's `agent start` and `agent prompt` do not apply to such a pane.
Slash commands, and therefore goals, are refused. A workspace trust dialog
stops the launch for human attention. The pane closes when the harness exits.
Slot problems are refused before any registry record or tab is created.

## Boxing a (sub)coordinator

```sh
agentctl start lead --harness claude --cwd /work/project/worktrees --project-box \
  --box-isolation root --brief 'Plan the release and launch one worker per slot'
```

`--project-box` (Rust edition, interactive Herdr sessions only) runs the agent in a wrkslots
coordinator box instead of a slot's box (see "Boxing a (sub)coordinator" in the wrkslots user
guide). It shares the `--slot` launch path. agentctl asks `wrkslots shell-command --box --name NAME
--cwd DIR --format json` for the command line (`--box-name`, default: the agent name; DIR is
`--cwd`, which is also the agent's working directory), and `--box-project DIR` names the project
root when `--cwd` is not inside it. Inside the box every slot, the wrkslots registry, the slots'
Git directories, and the agentctl registry in `--cwd` are writable (`--box-writable project`: the
whole project root), so the agent can run `wrkslots create` and launch its own workers with
`agentctl start --slot`. Those workers start in panes the Herdr server spawns, outside the
coordinator's box, and box themselves into their own slot. `--box-isolation root` uses the
`herdr-relay` adapter exactly as root slot isolation does, pinning the harness by the box scope's
cgroup. It conflicts with `--slot`.

## Agentcloud sessions

The Rust distribution can also launch a worker whose session runs in
agentcloud rather than on this machine. This gives a worker a machine of its
own, such as a freshly provisioned node with the checkout it needs, so that
checkout never has to exist locally. The Python distribution lists and validates
agentcloud profiles, marking them `"requires_edition": "rust"`, but refuses to
start them. `agentctl capabilities` lists the adapter under
`agentcloud`; it needs Herdr plus the separately installed and authenticated
`agentcloudctl` and `agentterm` commands.

Starting one is two steps, and `agentctl` performs both:

1. `agentcloudctl create` makes a root session. The agent name becomes its
   `--title` unless a title is configured, the brief becomes its durable `--prompt`, and the model,
   reasoning effort, driver, and node settings come from the profile or flags.
   `agentctl` requires stdout to be exactly one complete session UUID and
   records it before anything else happens; a shorter ID is refused, because the
   viewer would resolve it as a prefix. A failed create reports its stderr and
   opens no tab.
2. The agent's new Herdr tab runs `agentterm -s SESSION_ID`. `agentctl` pins the
   `agentterm` process as the tab's foreground process, so `status` can later
   say whether the viewer is still attached. A project `workspace` setting
   places this tab exactly as it places a local worker's tab. The same policy
   then governs the agent: `send` and terminal reads (`read --output tail` or
   `all`) refuse while the tab is missing or in another workspace, before any
   `agentcloudctl` call. `status`, `attach`, `wait`, `read --output last`, and
   `stop` remain available; `read --output last` is included because it reads
   the durable reply from agentcloud, not the terminal.

Both commands, and every later `agentcloudctl` call for that agent, receive the
same explicit `--ws-url`: the global `--agentcloud-url URL`, else
`$AGENTCLOUD_ORCHESTRATOR_URL`, else `agentcloudctl`'s documented production
endpoint. Passing it explicitly matters because `agentcloudctl` and `agentterm`
otherwise fall back to different settings, so the viewer could watch a
different orchestrator. The endpoint is recorded with the agent; later commands
always use it and refuse a different explicit `--agentcloud-url`. A record
written before endpoints were recorded still loads; its first `send` or `stop`
resolves the endpoint the same way and saves it, and `stop --skip-cloud-halt`
never needs one.

For an owner-configured profile, add an `agentcloud` block. This example
starts a Claude Code session on a freshly provisioned node; replace each
`<placeholder>` with a value from your deployment. `agentcloudctl models` lists
each driver's model keys and reasoning efforts:

```json
{
  "schema": "agentctl-profiles/v1",
  "profiles": {
    "cloud-worker": {
      "harness": "agentcloud",
      "mode": "interactive",
      "model": "<model-key>",
      "reasoning_effort": "high",
      "agentcloud": {
        "harness": "claude-code",
        "provision": true,
        "envspec": "<envspec>",
        "purpose": "<purpose>"
      }
    }
  }
}
```

```sh
agentctl start sub-cloud-worker --cwd . --profile cloud-worker \
  --brief 'Run hostname, then reply DONE.'
agentctl wait sub-cloud-worker --timeout 1800
agentctl read sub-cloud-worker --output last
agentctl stop sub-cloud-worker
```

The same launch without a profile uses explicit flags:
`--harness agentcloud --cloud-harness claude-code --provision --envspec
<envspec> --purpose <purpose>`, plus `--cloud-workspace /absolute/path` or
`--node-id ID` where needed. The `agentcloud` block accepts `harness`
(`native`, `claude-code`, `codex`, or `muse-code`; omitted means the server
default), `provision`, `envspec` and `purpose` (both require `provision`),
`workspace` (an absolute path on the node), `node_id` (which conflicts with
`provision`), and `title` (default: the agent name; `--cloud-title` on the
command line). `title`, `workspace`, and `node_id` may contain placeholders that
`start` expands: `{name}` is the agent name, `{host}` the local host name's
first label, `{fqdn}` the full local host name, and `{cwd}` the agent's
absolute directory. Any other `{...}` is refused, and the record and `status`
show the expanded values. A profile that binds the session to this host, uses
the agent's directory as the session directory, and titles it after both:

```json
"agentcloud": {
  "harness": "claude-code",
  "node_id": "{fqdn}",
  "workspace": "{cwd}",
  "title": "{name}-{host}"
}
```
 Profile `argv` and `--harness-arg` add literal `agentcloudctl
create` options, written as `--option=value`. Options that `agentctl` sets
itself, and endpoint or identity options that must match every later command
(`--ws-url`, `--client-cert`, `--client-key`, `--as-crewmate`), are refused.
`env` and `--env` apply only to the terminal tab.

Every later command uses agentcloud's durable interface rather than keystrokes:

| Command | Agentcloud behavior |
| --- | --- |
| `status`, `list` | One `agentcloudctl list` call per invocation, which does not attach to or wake a session. `agent_status` is `working` while a run is open, the listing's non-idle `activity` otherwise (for example `waiting` while the first prompt waits for a provisioned node), and `idle` when neither applies; `cloud` carries the listing's run, node, and status fields; `terminal` reports whether the tab and pinned `agentterm` process remain. A session missing from the lagged listing reports `unknown` with a `probe_error`. |
| `send` | `agentcloudctl send --end-of-turn`, with a 120-second bound, so input waits for the current turn instead of steering it. A leading `/goal` is sent without `--end-of-turn`. When the caller itself runs inside a different agentcloud session (`--from-session`, default `$AGENTCLOUD_SESSION_ID`), `agentcloudctl` refuses that send because it would speak as the human, so `agentctl` uses the attributed `agentcloudctl send-message` from the caller's session instead. The message arrives as an ordinary input and a `/goal` is not interpreted. An explicit human send (`--from-session ''`) from inside a different agentcloud session is refused before any call, because `agentcloudctl` deliberately rejects human input there and `agentctl` passes each child the session context it was given rather than removing it; the error prints the human command to run from a terminal outside any agentcloud session. The worker may try to reply to the sending session; its final text is still available through `read --output last`. `--message-id` becomes the idempotency key; without it `agentctl` generates one. Every failure, including a timeout after the service may already have accepted the input, prints that key and a shell-quoted retry command that repeats the same registry, endpoint, sender, verb, and key. The service deduplicates a key only for the same sender and verb, so follow that command exactly; its `--from-session ''` selects human attribution, which is refused when the command runs inside a different agentcloud session than the recipient's, so run a human retry from a terminal outside any agentcloud session. Delivery timeouts and `drain` do not apply. |
| `wait` | `agentcloudctl wait --until settle`. Success means the current or last run completed. A failed run, an interrupted run, and a timeout each exit 75 with a message naming which one happened. |
| `read` | `--output last` prints the last settled run's final reply from `agentcloudctl output`. `tail` and `all` read the `agentterm` tab. |
| `attach`, `pause`, `resume` | Focus the recorded tab; pause refuses automation input until resume. |
| `stop` | `agentcloudctl halt`, then close the recorded pane, then `agentcloudctl archive`, then archive the registry record. Immediately before closing, it checks again that the pane is in the same tab and workspace and runs the recorded `agentterm` or its idle shell; otherwise it refuses and keeps the record. It does not release the node reservation. |

A halt failure stops `stop` before anything local changes; retry it, since halt
is idempotent. Use `--skip-cloud-halt` only when the session no longer exists
or was retired elsewhere; it closes the tab and archives the record without
touching the session. `stop` does not release the node reservation, and neither
does `agentcloudctl node detach`; the orchestrator releases a provisioned
node's reservation on its own schedule after the halted session goes idle,
which can be long after `stop` returns. `agentcloudctl inspect -s SESSION_ID`
shows any remaining reservation. The stop result says which steps happened.

`agentcloudctl create` has a 300-second bound. `--startup-timeout` bounds only
the wait for `agentterm` to become the tab's foreground process. If create exits
9, the session exists but its node clause is unverified and the brief was not
queued. `agentctl` still attaches the tab and records the session, then exits
nonzero; inspect it, then send the brief or stop it, and do not start it again.
If the tab cannot be created or attached after a successful create, the record
keeps the session ID so that `stop` can halt it. The error prints the exact
manual viewer command, `AGENTTERM --ws-url ENDPOINT -s SESSION_ID` with the
recorded executable and endpoint, as does `attach` when the tab is gone. Every
printed `agentctl` recovery command names the registry it applies to. `goal`, `drain`, `bind-session`, and `adopt`
are refused for agentcloud agents; a goal travels as
`agentctl send NAME '/goal OBJECTIVE'`.

### Attach your own session

In the Rust distribution, an agent that itself runs in an agentcloud session
(`$AGENTCLOUD_SESSION_ID` is set) can show its own session in Herdr beside the
agents it starts:

```sh
agentctl attach-self coordinator --cwd .
```

Nothing is created or changed in agentcloud. `attach-self` registers the
session under the given name, opens a tab that runs `agentterm -s SESSION_ID`
with the same endpoint resolution as `start`, and returns the agent's status.
The tab goes to `--workspace-id`, else the configured project workspace, else
`HERDR_WORKSPACE_ID`, else the workspace labelled with the `--cwd` directory's
base name, which is created when absent. Later `start` calls from the same
session without an explicit or configured workspace open their tabs in that
workspace. A session that is already registered in this registry is refused
with its name. `stop NAME` closes the tab and archives the record but never
halts or archives the session, because it is the caller itself. Without
`$AGENTCLOUD_SESSION_ID`, `attach-self` is refused: a session in a local
terminal cannot be moved into Herdr.

## Observe and take over

```sh
agentctl read reviewer --lines 100
agentctl wait reviewer --timeout 60
agentctl pause reviewer
agentctl attach reviewer
agentctl resume reviewer
```

`read` returns a terminal snapshot for an interactive agent. It does not claim
that the snapshot is the final answer. `wait` observes readiness, which can occur
between steps of an active goal. Neither readiness nor successful delivery
proves completion of the task.

`pause` blocks automated input through the session manager; it lets an active
turn finish. `attach` focuses the terminal and does not itself transfer input
ownership. Pause before typing directly, coordinate with any separate input
producer, and clear unfinished composer text before `resume`. The native
harness remains usable directly in Herdr.

## Recipient identity, rename, and doctor

```sh
agentctl doctor
agentctl doctor --repair-labels
agentctl anchor reviewer
agentctl rename reviewer release-reviewer
```

Herdr routes input by pane ID, and a pane can come to hold another program: a
server restart can reuse the ID, or a different harness can start in the same
terminal. agentctl therefore checks the recipient immediately before **and**
immediately after every text or key it sends, including each repeated
submission key. The check covers the pane, its terminal and tab, the tab label
(which must equal the agent name), the Herdr agent name of an agent agentctl
started, and an anchor: the harness process pinned at start or adoption, or a
native session Herdr reported. A failure before anything was typed leaves the
message pending (exit 75).

After each prompt, agentctl reads the pane back. It counts the prompt's text in
the target pane (and, for prompts of 12 or more characters, in the other
registered agents' panes) before sending, and only text that appears after the
send counts. A peer pane Herdr reports as not found (its pane, tab or workspace
was closed) cannot receive the prompt: it is skipped and logged as
`peer-skipped` in `.agentctl/NAME/readback.jsonl`; any other failure to read a
pane before sending leaves the message pending. Delivery is reported only with evidence: the submission saw the
prompt printed in the target, or the prompt newly shows in the target's
scrollback within 5 seconds (for the `start --file` brief, within the
`--working-timeout`, since a harness that has just started can take longer to
print its first prompt), and the recipient check still passes. Text is compared
without whitespace and without the inline Markdown markup a harness may drop
when it prints a prompt (`` ` ``, `*`, `_`, `~`, `\`). Anything
else is quarantined under `queue/failed` (exit 76) for the operator, and is
never retried automatically:

- a check after the write fails, or the prompt newly shows in another
  registered agent's pane (`"probable_misroute": true`, with the panes involved
  in the error and in `.agentctl/NAME/misroutes.jsonl`);
- the prompt newly shows in the target and another pane, or in several panes;
- another registered agent's pane already showed the prompt and its count did
  not rise: an old copy may have scrolled out as a new one came in, so a new
  occurrence cannot be ruled out, whatever the window's length. This fails safe
  at a cost: a repeated identical prompt, or a broadcast while a peer that
  already shows the old copy keeps producing output, is quarantined and needs
  an operator resend. Removing that cost needs causal evidence from Herdr;
- the target never shows the prompt (delivery is unproven; recorded in
  `.agentctl/NAME/readback.jsonl`).

Nothing is typed into any other pane. Setting `AGENTCTL_MISROUTE_COUNTERMAND=1`
enables an experimental countermand instead: Esc and "Ignore the previous
message: it was sent to the wrong agent by agentctl." to the program that got
the prompt, under that pane's input lock and occupant checks, followed by one
retry. It is off by default.

When `herdr status server` prints `capabilities: input-expect`, each write also
carries the pinned terminal, and Herdr itself refuses a write to a pane that
holds another terminal, so replacement of the pane cannot receive the input.
Without that capability the window between the last check and Herdr's write
remains, and only the check afterwards detects a replacement. A program exiting
inside the same terminal is outside Herdr's knowledge in either case.

Records written before anchors existed refuse input until `agentctl anchor NAME`
pins the terminal and harness process now in the pane. Run it only after
looking at the pane and confirming it is that agent. A record whose anchor no
longer matches needs `--replace`, for the same reason.

`agentctl rename OLD NEW` changes the registry entry, the Herdr agent name, and
the tab label of a running native agent together; an adopted agent's alias is
renamed without touching its foreign presentation. Use it when an agent's warm
context is still useful but its purpose changed; otherwise start a new agent.
Rename refuses when NEW is registered, named in Herdr, or a tab label in the
workspace, and when OLD has no anchor. It writes a journal under
`.agentctl/.renames/` first; if it is interrupted, both names refuse every
command until the same `agentctl rename OLD NEW` is rerun, which verifies the
pane again and finishes. The record keeps its earlier names in `name_history`.
wrkslots slots, chat bindings, and scheduled prompts that name OLD are not
changed.

`agentctl doctor` compares every record with Herdr and prints one JSON report:
per record `workspace-missing`, `tab-missing` and `pane-missing` (Herdr has
closed it; doctor reports it and goes on to the other records), `harness-exited`, `harness-kind-mismatch`,
`harness-replaced`, `terminal-mismatch`, `tab-moved`, `label-mismatch`,
`herdr-name-mismatch`, `workspace-mismatch`, `rename-incomplete`,
`unanchored`, `duplicate-claim`; per workspace `label-collision` and the
informational `unmanaged-tab`. It exits 0 when clean and 1 with findings, and
changes nothing unless `--repair-labels`, which restores the label and Herdr
name of an owned agent only when every other check still passes. agentctl does
not schedule doctor; run it by hand, from cron, or from a coordinator's health
tick.

## Goals and native conversation identity

```sh
agentctl goal reviewer 'Complete the cancellation review'
agentctl goal reviewer
agentctl bind-session reviewer NATIVE_SESSION_ID
```

Setting a goal submits a native `/goal` command for Codex or an ordinary goal
instruction for other harnesses. It records the requested objective separately
from the native goal result. Native inspection requires a supporting harness,
a known conversation ID, and a working goal RPC interface. If the ID was not
reported automatically, use `bind-session` with the ID reported by that exact
agent. A shared working directory is not sufficient evidence of identity.

`goal`, `bind-session`, and `drain` apply to the native interactive adapter.
Headless turn runners expose their supported operations in `agentctl status`;
they do not acquire native goal RPC support by using Herdr for presentation.

An installation can select its native goal RPC with
`--goal-command-json '["/absolute/path/to/goal-rpc"]'`. `status` and `goal` expose
whether the native state is available; unverified intent is not reported as a
completed native goal.

## Stop and recover

```sh
agentctl stop reviewer
```

Every refused stop prints a reason and a `Recovery command:` line. The command
keeps the selected registry and Herdr executable, with shell quoting ready to
copy. Retirement advice includes the saved token. For an adopted record with no
saved shell identity, it also includes the raw record digest. Unverified
ownership, pending renames, and pending moves produce a `doctor` command for
inspection. An interrupted revive
provides the command that finishes its recorded transaction with the saved
token.

Stopping closes only a runtime created and owned by the session manager, then
archives its state. For an adopted `herdr-foreign` record, stopping means safe
unregistration: the manager verifies the supported shell's exact recorded
generation, verifies the live agent or proves the same pane and tab contain
that shell at an idle prompt, saves a terminal snapshot, repeats those proofs,
archives its record and queue, and leaves the pane, tab, and process untouched.
A live pane may have moved to another tab in the same workspace; its process,
session, and original shell identity must still match the record. An adopted
record without the shell's boot ID, PID, start ticks, and executable
device/inode cannot be retired automatically, whether the agent looks live or
absent. When the recorded pane is gone from Herdr's pane list and Herdr
positively reports its workspace, tab or pane as not found (`agentctl doctor`
shows `workspace-missing`, `tab-missing` or `pane-missing`), there is nothing
left to verify or close: `stop` archives the record as stopped, writes the
finding and Herdr's refusal to `stop.json` in the archived directory, reports
it as `source`, and exits 0. A Herdr outage is not such a report, so `stop`
still refuses.

A narrowly scoped recovery command exists only for an identity-less
`herdr-foreign` record whose raw JSON omits `foreign_shell_identity`. When the
record generation is verified, the refusal prints a command with the recovery
selector, its expected token, and its raw record digest.
`agentctl stop --help` describes these recovery options.

All three options are required together. Under the name and pane locks, the
manager re-reads the exact record bytes, requires the token and digest twice,
and requires one exact recorded pane/tab/workspace/cwd with no agent or native
session. The pane must contain a supported idle shell with no descendants, and
its boot ID, PID, start ticks, executable device/inode, and executable path must
remain identical around bounded output capture. It then atomically moves the
record, queue, and snapshot into the archive. It never signals, closes, moves,
renames, or reports the foreign pane. An explicit `null` identity field is not
the missing-key shape and is refused.

If a managed `herdr` agent has exited and its exact owned pane has returned to
an idle shell, normal stop requires an explicit generation assertion. The
refusal prints the command with the verified token filled in:

```sh
agentctl stop NAME --expected-token TOKEN
```

This path requires the exact one-pane tab, workspace, canonical cwd, absent
agent/session, descendant-free supported shell, and unchanged shell generation
around capture. It closes only the recorded pane. A moved pane, sibling pane,
replacement process, capture failure, or changed registry generation refuses
before close or archive.

An unavailable terminal server is not proof that an agent died. A lost terminal
view is not proof that a headless runner stopped. Preserve the registry and
inspect `status` when ownership checks refuse an operation.

Interactive prompt delivery distinguishes pending input from uncertain input.
Do not delete a queue or resubmit an uncertain request simply because no final
answer is visible. Check the native terminal, conversation, and durable artifact.
A generation change prevents old work from being silently redirected to a new
worker with the same name.

Most commands print JSON; `read` prints text and documentation commands print
prose. Successful operations exit 0. Pending interactive work exits 75 and
uncertain submission exits 76; failures include a diagnostic. Read the reported
outcome and saved artifact instead of treating every nonzero exit as a request
to submit again.

## Headless worker extension

When listed by `agentctl capabilities`, headless workers use the same names and
registry as interactive sessions:

```sh
agentctl start worker --mode headless --backend tmux --harness codex \
  --cwd /work/project --brief 'Investigate the failing tests'
agentctl send worker 'Summarize the remaining failures'
agentctl read worker --output last
agentctl read worker --output since_turn --since-turn 2
```

The long-lived runner invokes structured harness turns and retains the native
conversation ID between turns. The presentation terminal displays status and
transcripts. Pause automated input before direct intervention. `reset` clears
an idle worker's conversation context; `repair` recreates a missing presentation
for a live runner. `migrate --backend herdr|tmux` moves an idle worker while
preserving its conversation and queue, subject to supported mode transitions.
The source and destination must retain compatible harness permissions.

The worker inbox marks claimed turns before execution. After an interruption,
inspect uncertain turn state and the native conversation rather than assuming
that rerunning a command is harmless. Stop and migration operations verify
ownership and preserve recoverable state on failure.

Muse headless turns put the subcommand first—`muse exec ... --json`—and pass an
agentctl-generated UUID as `--session-id` on every turn. A `--` option terminator
immediately before the literal prompt prevents prompt text such as `--help` or
`--disable-sandbox` from becoming a Muse option. They never use Muse's
separate interactive `resume` subcommand. The runner accepts only schema-version-1 JSONL
events for that exact session, requires monotonically increasing per-turn
sequence numbers, one accepted command, and one terminal outcome. Event count,
event size, final-answer size, wall time, and stderr retention are bounded.
Missing, duplicate, malformed, or cross-session terminal events produce an
explicit protocol error rather than an apparent successful answer.

## Chat and MCP extensions

`agentctl chat quickstart` and `agentctl chat userguide` describe the Chat
service shipped by the installed edition. Both accept authorized messages into
durable state before waiting for the coordinator to become ready. A configured
reaction means the bridge ingested the message; the final answer arrives
separately. Reaction failures remain visible and retryable without aborting
prompt delivery or replies.

The Chat bridge is a separate, long-running process. It can target one coordinator
without any worker team. It requires an interactive coordinator in Herdr and
uses terminal input; a command or socket transport changes Google Chat access, not the
harness connection. By default, the coordinator brackets its final answer with
unique tags from the prompt. The bridge waits on a Herdr subscription, captures
that block, and posts it durably; no reply file or CLI call is needed from the
agent. The polling edition also supports explicit file replies with `reply_mode:
"file"`; the subscription-plugin edition uses only tagged terminal capture.
Retained terminal history is bounded, so capture failures remain visible for
inspection. The bridge does not supervise the coordinator's process.

In the Python edition, for an operator-created Herdr tab, `agentctl chat launch --config chat.json`
provides the supervised exception to that separate-process model: it discovers
the current pane and workspace, runs the native coordinator there, and owns a
bridge child for exactly that coordinator's lifetime. The inherited
`HERDR_WORKSPACE_ID` also keeps the coordinator's default `agentctl start`
subagents in the same Herdr workspace. The reusable launch config contains Chat
authority and transport settings; the command supplies the target identity.

The Python edition's built-in Chat transport polls the public REST API. An optional
`event_command` connects an operator-supplied event stream; messages then wake
the bridge immediately, while ACKs, Herdr prompt delivery, final replies, and
recovery scans run independently. A durable cursor and message IDs allow safe
replay after reconnecting. `run --reconcile-interval` controls REST
reconciliation in streaming mode (default: 300 seconds); the local durable-state
recovery pass remains fixed at five minutes. Without an event adapter,
`run --interval` controls the delay after each completed polling cycle
(0.1–86400 seconds; default: 3600 seconds). Failure backoff never polls more often
than the configured interval and grows toward a one-day ceiling after long-interval
failures. The Chat user guide documents the adapter protocol.

The Rust edition instead consumes an installed subscription plugin through its
event-driven protocol. It uses an independently configured, bounded one-shot
NDJSON helper for optional reactions and replies. `chat init` validates the
plugin, named Herdr target, helper, and authority before creating state; `chat
run` owns the provider and Herdr subscriptions; `chat tick` performs one bounded
recovery pass; `chat status` is read-only; and `chat close` retires one exact
reply route. Its dedicated `chat userguide` documents the schemas and service
manager limits. A protocol-v1 provider Gap is journaled without advancing or
acknowledging its cursor and makes `run` stop fail-closed; automatic reconnect
is refused until a future explicit repair mechanism can prove completeness.
Status distinguishes a prepared local commit attempt from an exact confirmed
plugin callback receipt. Explicitly closed, fully delivered requests outside
the current inclusive cursor are retired through a bounded fsynced audit and
replay-guard ring rather than exhausting the 2,048 active-request cap.

One `run` or `tick` process owns each Chat state directory. Stop `run` before
using `tick` for manual recovery. In the polling edition, explicit `chat reply`
submissions remain available while `run` owns the state; a private local
notification wakes that runner after the submission is durable, and its
unconditional five-minute recovery pass covers a missed notification or
restart.

The polling edition's thread replies include a command hint for the nearest ten prior messages:
`agentctl chat context --state DIR --request KEY_OR_UNIQUE_HEX_PREFIX --limit 10`.
It reads one page from the request's exact thread and time cutoff, prints it
chronologically, and supplies a cursor for older context. This command needs
Chat access and saved bridge state, but no live Herdr pane.

`agentctl mcp --registry /absolute/path` serves session operations over stdio
when the MCP extension is available. The coordinator can use either CLI or MCP;
workers do not need individual Chat connections. Its `agent_adopt` tool requires
the same `name`, `pane`, `workspace`, `cwd`, and `harness` assertions and accepts
the same optional already-reported `session` assertion as the CLI.

## Compatibility entry points

The agent-control package supplies `herdr-agent` as a compatibility entry point.
Installations with worker and Chat extensions also supply `herdr-subagents` and
`herdr-chat`. Prefer the single `agentctl` command for new integrations:

| Existing entry point | Unified operation |
| --- | --- |
| `herdr-agent start NAME` | `agentctl start NAME` |
| `herdr-agent send --name NAME TEXT` | `agentctl send NAME TEXT` |
| `herdr-agent status --name NAME` | `agentctl status NAME` |
| `herdr-subagents up NAME ...` | `agentctl start NAME --mode headless ...` |
| `herdr-subagents send NAME ...` | `agentctl send NAME ...` |
| `herdr-chat COMMAND ...` | `agentctl chat COMMAND ...` |

Compatibility commands retain their state-directory defaults: `herdr-agent`
uses `.herdr-agents` for managed sessions and `.herdr-agent` for an explicit
delivery queue; `herdr-chat` uses `.herdr-chat`. The worker compatibility command
uses `HERDR_SUBAGENTS_HOME`, or `$XDG_STATE_HOME/herdr-agent/foreign` (default:
`~/.local/state/herdr-agent/foreign`).

`herdr-agent start` continues to accept raw harness policy when the matching
structured selector is absent, including a raw Claude `--effort=high`, and it
accepts non-overriding Codex `-c` assignments. With structured `--model`, a raw
model option, a Codex `model=...` assignment, or a Codex profile selector
(`-p`/`--profile` or `-c`/`--config profile=...`) is refused before registry
state is created. The shared API applies the same rule to an explicit structured
reasoning effort; `herdr-agent` has no structured effort flag, so its raw effort
arguments remain unchanged. A structured `--resume` also cannot be repeated
through raw Codex `resume` or Claude `--resume`/`--continue` selectors.

Use `agentctl --registry .herdr-agents` to continue a managed interactive
registry. An independent worker-compatibility registry retains its own command
and environment until those workers are stopped and started through the unified
interface. Creating a new registry does not adopt an agent by matching its name;
use `agentctl adopt` with the explicit live identity assertions above.
Low-level compatibility arguments remain discoverable with each entry point's
`--help`.

Use `agentctl COMMAND --help` for argument descriptions, defaults, units, and
examples. These installed documentation commands are the operator reference;
they do not require access to a source checkout or a web manual.
