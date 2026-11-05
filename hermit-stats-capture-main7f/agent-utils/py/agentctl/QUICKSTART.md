# agentctl quickstart

Use one named-session interface to delegate work to persistent coding agents
while keeping their terminals available for direct inspection.

1. Install Herdr and an authenticated Codex, Claude, or Muse harness. Start a Herdr
   workspace for the project. Check this installation's adapters:

   ```sh
   agentctl capabilities
   agentctl skill install
   ```

   Muse skills are installed through Muse's native user-scope skill manager;
   Codex and Claude receive the same bundled file in their skill roots.

2. Start a worker in your project. Choose an accessible model with `--model` if
   the harness default is unsuitable:

   ```sh
   agentctl start reviewer --harness codex --cwd . \
     --brief 'Review the current changes and report concrete problems'
   agentctl list
   agentctl status reviewer
   ```

   If a running record's harness was killed, preview recovery and resume its
   recorded conversation in a fresh tab. The old queue and quarantine are
   archived without replaying prompts:

   ```sh
   agentctl revive reviewer --dry-run
   agentctl revive reviewer
   agentctl revive --all --dry-run
   ```

   A project can keep owner-specific launch choices in the private, ignored
   `.agentctl/profiles.json` file. Inspect safe profile metadata, then select
   one without restating its model, environment, or permission arguments:

   ```sh
   agentctl profiles --cwd .
   agentctl start reviewer --cwd . --profile preferred-reviewer
   ```

   Resume a saved local interactive Claude, Codex, or Muse conversation with
   that profile's launch settings:

   ```sh
   agentctl start reviewer-again --cwd . --profile preferred-reviewer --resume SESSION
   ```

   An optional top-level `"workspace": "project-agents"` setting makes that
   Herdr workspace label authoritative for starts and automated input. Status
   and stop remain available for diagnosis and retirement.
   Move an existing owned native agent into it without restarting the process:

   ```sh
   agentctl move reviewer
   ```

   When `agentctl capabilities` lists `agentcloud`, a worker can instead run
   on its own agentcloud node, for example one provisioned with the checkout it
   needs, while its terminal viewer opens in a Herdr tab. `agentctl` runs
   `agentcloudctl create`, records the session ID, and starts
   `agentterm -s SESSION_ID` in the tab. `stop` halts and archives the session
   but does not release the node reservation:

   ```sh
   agentctl start sub-cloud-worker --harness agentcloud --cloud-harness claude-code \
     --provision --envspec ENVSPEC --brief 'Run hostname, then reply DONE.'
   agentctl wait sub-cloud-worker --timeout 1800
   agentctl read sub-cloud-worker --output last
   agentctl stop sub-cloud-worker
   ```

   The session's orchestrator is `--agentcloud-url URL`, else
   `$AGENTCLOUD_ORCHESTRATOR_URL`, else `agentcloudctl`'s documented production
   endpoint. It is recorded and passed to both `agentcloudctl` and `agentterm`,
   so set it once at start when you use a non-default orchestrator:

   ```sh
   agentctl --agentcloud-url wss://orchestrator.example/ws/chat \
     start sub-cloud-worker --harness agentcloud --cloud-harness claude-code
   ```

   `agentctl userguide` describes the matching profile block and how each
   command behaves for an agentcloud session.

   To keep an agent that is already running in Herdr, adopt its exact live
   identity instead. All four assertions are required; adoption changes neither
   the pane nor its process:

   ```sh
   agentctl adopt reviewer --pane w1:p2 --workspace project \
     --cwd /work/project --harness codex
   ```

   Muse can also be adopted with `--harness muse`. Adoption requires a stable
   foreground process and terminal identity; automated delivery uses the same
   verified composer and Auto-review policy as an owned Muse session.

3. Send follow-up work and inspect progress:

   ```sh
   agentctl send reviewer 'Check cancellation and restart behavior too'
   agentctl read reviewer --lines 100
   agentctl goal reviewer
   ```

   Busy input can wait. Use `--ready-timeout 0` to leave it durably queued, then
   `agentctl drain reviewer` to attempt delivery. A native goal is authoritative
   only when the session is bound and its harness supports goal inspection.

4. To type directly, pause automated input first. Let any current turn finish,
   open the terminal, and clear any unfinished draft before resuming:

   ```sh
   agentctl pause reviewer
   agentctl attach reviewer
   agentctl resume reviewer
   ```

5. Finish with `agentctl stop reviewer`. This stops a runtime created by
   `agentctl` and archives its session state. A refusal prints its reason and
   a `Recovery command:` line with the selected registry and verified identity
   assertions. Run that command to recover or inspect the record.
   For an adopted agent it only
   unregisters and archives the control state; the foreign pane and process keep
   running. Adoption accepts only a supported shell process and records its
   exact generation. Normal unregistration requires that same shell generation before
   and after the final snapshot whether the foreign agent is live or has
   exited; the exited path additionally requires the recorded idle shell.
   Records without that process identity refuse normal retirement. A dead,
   identity-less adoption has one explicit recovery path, requiring the exact
   current registry generation and raw record digest. Run
   `agentctl stop --help` for its required recovery selector, token, and digest
   options.

   That path requires the exact recorded one-pane tab, an absent agent/session,
   and the same supported descendant-free idle shell before and after capture;
   it archives control state without touching the foreign runtime. An owned
   managed agent that has returned to its shell likewise requires
   `--expected-token TOKEN` before `stop` can close its exact pane.
   After a Herdr restart, a running adopted record with a saved harness identity
   can be archived once that exact harness generation is proved dead. Its
   printed recovery command uses `--retire-dead-adoption` with the exact token
   and raw record SHA-256. That recovery preserves all original record, output
   and queue files, including quarantine, and works without contacting Herdr.
   Exiting the CLI or closing its caller does not stop the worker.

State defaults to `.agentctl` in the current directory. Use the same
`--registry /absolute/path` across callers. Workers share the working directory
you choose; use separate worktrees when their edits need isolation.

`agentctl userguide` explains lifecycle and recovery. `agentctl start --help`
and the other subcommand help pages describe every option. Worker, Chat, and
MCP extensions are installation-dependent; inspect `agentctl capabilities`
before using their commands.

Chat transports vary by installation. When `agentctl capabilities` lists the
event-driven subscription service, it targets an already registered interactive
agent:

```sh
agentctl chat quickstart
agentctl --registry /work/project/.agentctl chat init \
  --config chat.json --bridge-state /home/me/.local/state/agentctl/project-chat
agentctl --registry /work/project/.agentctl chat run \
  --bridge-state /home/me/.local/state/agentctl/project-chat
```

When `agentctl chat --help` lists `launch`, the installed polling transport can
instead launch a coordinator directly in the current Herdr shell pane for the
lifetime of that session:

```sh
agentctl chat launch --config chat.json --model gpt-6-astra
```

Run `agentctl chat quickstart` for the installed edition's compact
configuration and `agentctl chat userguide` for its authentication, recovery,
and service model. A subscription-plugin bridge is a separate foreground
process controlled by `chat run`; it does not use `chat launch`.

The Chat bridge accepts messages from configured senders, acknowledges intake
with 🤖, and sends the agent's tagged replies back to their originating chat
thread. One request can receive multiple replies, including progress updates.
The agent normally needs no reply-file write or reply command. Thread
replies also include a command hint for reading the nearest ten prior messages.
A polling transport can consume the public Google Chat API or its configured
`event_command`. A subscription transport blocks on the selected plugin. In
both cases ACKs, prompt delivery, and recovery scans run independently.
