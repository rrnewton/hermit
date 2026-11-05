# agentctl

Keep several coding agents working on a project, send them follow-up tasks, and
open their terminals whenever you need to inspect or steer them. A person or a
coordinator agent uses the same named-session interface.

```sh
agentctl quickstart
agentctl start reviewer --cwd . --brief 'Review the current changes'
agentctl send reviewer 'Focus on cancellation and restart behavior'
agentctl attach reviewer
```

Interactive sessions keep the native Codex, Claude, or Muse terminal running in
[Herdr](https://github.com/herdrdev/herdr), installed separately. Authenticate
and install the chosen harness separately too. The command stores session
identity and durable delivery state locally; it needs no hosted account or
always-running manager process.

Muse uses a pane-native adapter until Herdr recognizes it as a native agent
kind. Startup verifies the exact foreground executable and idle composer;
delivery proves literal draft insertion and then that the prompt moved from the
composer into the transcript after Enter.
Provider downgrades such as requested `ultra` resolving to `xhigh` remain
visible in status metadata.

Some installations also provide resumable headless turns in Herdr or tmux, a
polling Google Chat coordinator bridge, and an MCP interface. A subscription
plugin installation instead provides a durable event-driven Chat host with an
independently configured outbound helper. In either bridge, the agent can send
progress updates and multiple replies to one request from its terminal without
writing a reply file; reply tags are independent of the provider.
Thread replies include a command hint for reading earlier context.
Run `agentctl capabilities` to see the modes,
backends, harnesses, and services included in your installation. A headless
worker's terminal displays its transcript; it is not a native harness TUI.

The documentation is installed with the command and works offline:

- `agentctl quickstart` gets the first session running.
- `agentctl userguide` explains ownership, recovery, goals, and available extensions.
- `agentctl profiles --cwd DIR` lists owner-defined launch profiles without
  printing their environment or raw arguments.
- `agentctl skill install` installs this interface's bundled skill for Codex,
  Claude, and Muse without overwriting divergent content. Muse uses its native
  managed skill installer; an identical managed copy is not reinstalled.
- `agentctl COMMAND --help` describes that command's arguments and examples.
- `agentctl chat quickstart` and `agentctl chat userguide` explain the Chat
  service shipped by the installed edition.
- When listed in `agentctl chat --help`, `chat launch --config chat.json` starts
  a coordinator in the current Herdr shell pane with its bridge supervised for
  that session. Subscription-plugin installations instead use an already
  registered agent and the separate `chat init`, `chat run`, and `chat tick`
  lifecycle.

Paused or uncertain work remains visible. A delivered instruction or an idle
terminal does not establish that the agent's task is complete.
