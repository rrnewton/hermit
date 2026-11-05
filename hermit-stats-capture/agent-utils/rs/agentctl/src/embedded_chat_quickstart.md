# agentctl chat quickstart

Connect one registered interactive Herdr agent to an installed event-driven
subscription plugin. The bridge keeps provider replay state, prompt-delivery
state, reply fences, and stable outbound operation IDs under one private state
directory.

1. Confirm that the provider plugin is safely discovered and that the target
   agent is live:

   ```sh
   agentctl capabilities
   agentctl --registry /work/project/.agentctl status coordinator
   ```

2. Create an owner-only `chat.json`. Replace every example authority and path:

   ```json
   {
     "subscription_plugin": "provider-events",
     "subscription_environment": [],
     "channel_ids": ["spaces/example"],
     "allowed_senders": ["users/owner"],
     "agent_name": "coordinator",
     "agent_label": "codex coordinator",
     "outbound_enabled": true,
     "ack_reaction": null,
     "backend_configuration": {
       "schema": "provider.example/v1",
       "data": {}
     },
     "outbound_command": {
       "executable": "/absolute/path/to/reply-helper",
       "arguments": [],
       "environment": [],
       "timeout_millis": 30000,
       "shutdown_grace_millis": 2000
     }
   }
   ```

   The helper is independent of the inbound plugin. Omit `outbound_command`,
   set `outbound_enabled` to `false`, and keep `ack_reaction` null for an
   intentionally inbound-only bridge. Add only subscription-plugin environment
   variable names to `subscription_environment`; every named value must exist
   when the service starts, while its value is never saved in bridge state.

3. Initialize once, then run the foreground service:

   ```sh
   chmod 600 chat.json
   agentctl --registry /work/project/.agentctl chat init \
     --config chat.json \
     --bridge-state /home/me/.local/state/agentctl/project-chat
   agentctl --registry /work/project/.agentctl chat run \
     --bridge-state /home/me/.local/state/agentctl/project-chat
   ```

   Stop with SIGINT or SIGTERM. A service manager may restart the same `run`
   command against the same state directory.

Use `agentctl chat status --bridge-state DIR` for a read-only durable status
snapshot. Stop `run` before `agentctl chat tick --bridge-state DIR`, which runs
one bounded recovery pass. `agentctl chat publish --bridge-state DIR
--channel-id CHANNEL --request-id UUID TEXT` is the explicit operator-only way
to send a root message; it does not mutate bridge state or act as an event-loop
reply. `agentctl chat thread --bridge-state DIR --thread THREAD` prints one
thread's retained messages, oldest first; a request prompt for a reply in an
existing thread prints this command, with absolute paths.

Each request prompt gives the agent one short reply ID, such as `001`, for every
reply to that request. The numbers count up across requests and are kept in
`reply-aliases.json` in the state directory; one that the agent's pane shows
when a request gets its number is skipped, and none is reused unless that file
is deleted or replaced by an older copy, or the bridge gets a new state
directory. A block under the number of a request whose prompt still waits in the
agent's queue is not posted to that request, then or later, apart from gaps the
userguide lists, and the agent gets a routing-error prompt about it when a
recovery scan reads it. While the closing line of a reply under a short ID is in
view, `run` reads the pane every 2 seconds, because herdr raises no event for
the next reply under the same ID. It also reads the pane every 2 seconds while
the agent works and some request is open, so that a reply block the agent's
later output pushes off the screen is normally read before it goes; the
userguide says what these reads cost, how `run` tells that the agent works, and
when such a block is still lost. Each reply block the agent writes for an open
request is posted once for each distinct text, compared as the userguide
describes, whichever of that request's reply IDs it uses. Rows that start like a
prompt the agent received, or like a tool call's output, are skipped with the
rows that continue them, so a reply block quoted in a message to the agent is
not posted; the userguide describes these rules and where they fail. The bridge
reads only the screen of a pane that herdr reports keeps no scrollback, such as
a Claude Code pane, with two exceptions the userguide gives, so there a block is
posted only when a capture shows the first row of its message and the whole
block, and a block taller than the screen is not posted. A block under a reply
ID that matches no request the bridge knows, or one a capture shows only in
part, is not posted, and the agent gets a routing-error prompt about it, usually
once, as the userguide describes. A block under a well-formed ID of a closed
request is ignored. For an unmatched ID, the prompt lists the open requests
whose prompts reached the agent's queue: first those with no reply yet, most
recent first, and then those already answered.
A block that cannot be posted, such as an empty or oversized one, is skipped
with one log line that begins with the UTC time and then
`agentctl: chat reply capture:`; nothing the agent prints stops `run`.
Every `run` log line begins with the time, and the provider logs each
subscription it opens and each reconnect it waits for.
`agentctl chat userguide` documents plugin safety, the outbound NDJSON contract,
exact local commit receipts, reply capture, explicit route closure and bounded
retirement, fail-closed provider gaps, recovery, and service-manager limits. A
status with `healthy: false` and an unresolved gap is not live success; protocol
v1 intentionally refuses automatic reconnect.
