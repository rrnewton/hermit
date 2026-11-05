# agentctl chat — durable event-driven coordinator bridge

`agentctl chat` connects an already registered interactive agent to a chat
subscription plugin. The plugin supplies normalized inbound events through the
provider-neutral `chat-subscription` traits. A separately configured one-shot
command performs replies and reactions. Neither executable obtains authority
from its manifest: the operator chooses routing, sender allowlists, environment
names, and the exact outbound helper path.

The bridge admits each provider batch and its replay cursor durably before it
acknowledges that batch. It writes a bounded `prepared` receipt first, calls the
plugin commit callback, waits for the exact matching `Committed` frame, and only
then atomically replaces that slot with a `committed` receipt containing the
delivery ID, provider sequence, host batch sequence, cursor, event count, and
timestamp. A `prepared` receipt is never reported as confirmed. This artifact
proves that the local plugin callback was released and returned exact protocol
confirmation; it does not prove a later provider network or server-side action.
The bridge then sends the request to the configured named agent.
Replies use the exact `CHAT_REPLY` fence supplied in the prompt and are retained
with one stable outbound UUID before any send attempt. A restart retries the same
operation identity; an unknown transport result is never converted into success.

## Configuration

Create a private owner-only JSON file (normally mode `0600`):

```json
{
  "subscription_plugin": "provider-events",
  "subscription_environment": ["PROVIDER_CERT", "PROVIDER_KEY"],
  "channel_ids": ["spaces/example"],
  "allowed_senders": ["users/owner"],
  "agent_name": "coordinator",
  "agent_label": "codex coordinator",
  "outbound_enabled": true,
  "ack_reaction": "🤖",
  "backend_configuration": {
    "schema": "provider.example/v1",
    "data": {}
  },
  "outbound_command": {
    "executable": "/absolute/path/to/reply-helper",
    "arguments": [],
    "environment": ["PROVIDER_ACCOUNT"],
    "timeout_millis": 30000,
    "shutdown_grace_millis": 2000
  }
}
```

The subscription plugin is discovered below `$AGENTCTL_HOME/plugins` (default
`~/.agentctl/plugins`) using its private manifest. The bridge revalidates and
pins the manifest directory and executable before every generation. Provider
credentials do not belong in the manifest or saved bridge state. When a plugin
needs credential paths or tokens from the service environment, list only their
variable names in `subscription_environment`. Names must be unique shell
identifiers, at most 128 bytes each, with no more than 64 names. Every named
value must be present before the plugin can be launched. The host clears the
plugin environment, restores its small documented non-secret baseline, then
adds exactly these operator-selected names. Values are neither serialized nor
reported by `chat status`; a plugin manifest has no authority to add names.

The outbound helper receives exactly one newline-terminated JSON request and
must emit exactly one newline-terminated JSON response. It is started inside the
same reviewed pidfd/private-process-group supervisor used for subscription
plugins, with a cleared environment plus only the configured names. The host
sets `AGENTCTL_PROCESS_SUPERVISED=1`. The helper must not create another process
group. Its operation deadline is positive and at most 30 seconds. At service
startup the host validates and hashes a native helper of at most 64 MiB, copies
those exact bytes into a sealed in-memory executable, and retains that image for
the generation. Reply and reaction operations neither reopen nor rehash the
source path.

If outbound messages use an allowed sender identity, prevent their labelled text
from becoming new requests with a runtime-only exclusion, for example
`chat run --bridge-state PATH --ignore-text-prefix '[assistant'`. Repeat the
option for up to 32 prefixes. Matching is literal and case-sensitive after
leading Unicode whitespace; each prefix must be nonempty, at most 256 UTF-8
bytes, and contain no control characters. The default excludes nothing.
Ignored messages still participate in the original provider batch fingerprint,
replay boundary, and cursor commit, but create no request, reaction ACK or pane
delivery. This does not suppress already admitted work. Supply the options on
every run; they do not change the persisted configuration or apply to `chat tick`.

In `chat run`, a generation-owned worker queues reaction ACKs as soon as the
inbound batch is durably admitted and its provider commit is confirmed. Pane
delivery, later intake, output capture and replies do not wait for that worker.
It runs one ACK at a time through a bounded queue; overflow stays in durable
state for recovery. Failed or uncertain ACKs retain their operation ID and wait
at least 60 seconds before an in-process retry. A restart reconciles pending
ACKs using those same IDs. `chat tick` still completes its bounded ACK work
before returning. During service shutdown, queued work remains pending and an
already admitted ACK is owned until bounded completion or uncertain cleanup.

To run intentionally without reactions or replies, set `outbound_enabled` to
`false`, set `ack_reaction` to `null`, and omit `outbound_command`. The delivered
prompt then explicitly identifies the bridge as inbound-only and forbids reply
fences.

## Initialize and run

The named agent must already exist in the selected registry. Every channel in
`channel_ids`, and every thread within it, routes to that one agent. To give
several agents their own conversations, give each agent its own channel, its
own config, and its own bridge state. Two bridges cannot split one shared
channel by thread. Initialization checks the live pane, plugin installation,
outbound executable, and complete configuration before it creates state:

```sh
chmod 600 chat.json
agentctl --registry /work/project/.agentctl chat init \
  --config chat.json \
  --bridge-state /home/me/.local/state/agentctl/project-chat

agentctl --registry /work/project/.agentctl chat run \
  --bridge-state /home/me/.local/state/agentctl/project-chat
```

`run` owns an exclusive state lease. Provider intake blocks on the plugin event
stream; it does not poll a REST listing. Terminal reply capture blocks on
Herdr's `events.subscribe` socket with bounded groups of current closing-fence
IDs and a generic predicate for unavailable IDs. The current groups rearm when
reply routes change, so a consumed closing fence left on screen cannot mask the
next reply. A closing fence under an earlier ID of an open request routes to
that request too, because the block may hold new text. Only the generic
predicate matches such an ID, and it stays matched while any closing fence
remains on screen, so such a block is usually captured at the next recovery
scan rather than at once. Exact IDs route through the in-memory durable-state
index; all 2,048 active requests fit within the subscription budget of 128
predicates, 32 KiB per predicate and 96 KiB total. Provider notices and
SIGINT/SIGTERM interrupt that wait through a local wake descriptor. While the
agent works and a request is open, `run` also reads the pane itself every 2
seconds, as described below. A disk-backed terminal and delivery reconciliation
occurs every 300 seconds by default and can be changed with
`--reconcile-interval`. Every 10 seconds `run` also scans the request records
for prompts left waiting to be typed and tries them again while the agent is
idle or done, as described under Prompt delivery below.

A provider `Gap` is a terminal continuity warning in protocol v1. Its reason does not
contain a recoverable range or completeness proof, so the host journals the
incident, keeps the prior safe cursor, sends no commit, cancels that provider
generation, and exits degraded. `chat status` reports `healthy: false` and the
exact unresolved incident; `chat run` refuses automatic reconnect. One later
message or checkpoint does not prove that a missing interval was recovered.

An operator may approve a retry only when the saved boundary is exactly one
committed `Checkpoint`, the gap proposes that same cursor, and independent
provider evidence establishes that retrying that fixed cursor is safe. Stop the
runner and review its backend's explicit retention-boundary recovery policy
first. The host does not interpret a provider's free-text reason as proof.

```sh
agentctl chat retry-checkpoint-gap \
  --bridge-state /home/me/.local/state/agentctl/project-chat \
  --expected-gap-sha256 "$GAP_SHA256" \
  --expected-checkpoint-sha256 "$CHECKPOINT_SHA256" \
  --expected-configuration-sha256 "$CONFIGURATION_SHA256" \
  --keep-cursor "$COMMITTED_CURSOR" \
  --evidence-file /home/me/private/provider-evidence.json \
  --evidence-sha256 "$EVIDENCE_SHA256"
```

Supply lowercase SHA256 digests of the exact reviewed `gap.json`,
`checkpoint.json`, `bridge.json`, and evidence file bytes. Evidence must be a
private nonempty JSON object of at most 64 KiB. It is retained as an explicit
operator attestation, not a host-verified claim about provider history. The
command verifies the stopped runner lease, all local digests, and the exact
committed checkpoint receipt; it refuses message-bearing boundaries, including
messages excluded by a runtime prefix filter.

Approval writes only a bounded audit under `gap-retries/`; it preserves the
cursor, request bytes, UUIDs, and unresolved diagnostic. Status reports
`gap_retry_approved: true` and `healthy: false`. The next generation may admit
only the identical checkpoint at the kept cursor. A new exact provider commit
resolves the gap and clears reconciliation; an interrupted resolution is
repaired from that new committed receipt. A new gap revokes the approval before
it is published. Audits retain original state and evidence without eviction,
with a limit of 64 records and 512 KiB per record. This procedure cannot reset
to the provider head, accept loss, replay quarantined work, or claim recovery
before provider confirmation.

For a committed boundary containing messages, use the separate explicit
`chat retry-boundary-gap` operation only after reviewing evidence that the
provider can replay the **exact original batch** at the saved cursor:

```sh
agentctl chat retry-boundary-gap \
  --bridge-state /home/me/.local/state/agentctl/project-chat \
  --expected-gap-sha256 "$GAP_SHA256" \
  --expected-checkpoint-sha256 "$CHECKPOINT_SHA256" \
  --expected-configuration-sha256 "$CONFIGURATION_SHA256" \
  --keep-cursor "$COMMITTED_CURSOR" \
  --evidence-file /home/me/private/provider-evidence.json \
  --evidence-sha256 "$EVIDENCE_SHA256"
```

It applies the same stopped-runner, digest, evidence and committed-receipt
checks. The audit additionally records exact-boundary authority. The complete
ordered event fingerprint, event count and every retained message guard must
remain equal, including original full provider payloads. Matching only a
message ID or text is insufficient; current REST metadata may differ from the
original event. A checkpoint cannot replace a message-bearing boundary, even
when a runtime prefix filter excluded its message from actionable work.

Approval does not fetch messages, change the cursor, clear the gap, reset to
provider head, or accept loss. The next generation must replay the original
inclusive boundary; any mismatch stops without provider acknowledgement.
Only a newer exact committed replay resolves the incident. Same-cursor replay
creates no new requests or ACK/reply UUIDs, even if the current prefix policy
differs; existing quarantines remain intact. New gaps revoke this authority,
and the same receipt-based crash recovery and bounded audit retention apply.
The checkpoint-only command continues to refuse message-bearing boundaries.
An older host that cannot read an exact-boundary audit fails closed; keep the
matching host available through retry and confirmation.

For a bounded local work recovery pass while the daemon is stopped (`tick` does
not repair provider gaps):

```sh
agentctl --registry /work/project/.agentctl chat tick \
  --bridge-state /home/me/.local/state/agentctl/project-chat
```

Inspecting status is provider- and Herdr-free and does not perform recovery
writes:

```sh
agentctl chat status \
  --bridge-state /home/me/.local/state/agentctl/project-chat
```

Inspect one exact active retained request for latency/audit evidence without a
provider, outbound helper, Herdr call, or state write:

```sh
agentctl chat inspect \
  --bridge-state /home/me/.local/state/agentctl/project-chat \
  --request 64_LOWERCASE_HEX_CHARACTERS
```

The stable output schema is `agentctl-chat-request-inspection/v1`. It contains
the request phase and source IDs; `provenance.host_batch_sequence`,
`provider_sequence`, `delivery_id`, and cursor; `timestamps.admitted_at_millis`,
`delivery_started_at_millis`, `delivered_at_millis`, `ack_started_at_millis`,
and `ack_completed_at_millis`; delivery and acknowledgement receipts; and a
bounded `replies` array with each ordinal, phase, operation ID, provider message
ID, `captured_at_millis`, and `sent_at_millis`. Start timestamps record the first
durable attempt. Delivery, ACK, and reply completion timestamps remain null
after failed or outcome-unknown operations and are written only with positive
Herdr/provider evidence. The command accepts one exact key and reports only an
active retained request. An acceptance harness that later calls `chat close`
must copy and fsync this inspection document first.

The first line of each request prompt opens with the message's own create
time, as `Sent 2026.10.07:08:45 EDT.`, in the bridge service's local time zone:
the one the C library resolves from `TZ` when it is set, otherwise from the
host's `/etc/localtime`. Set `TZ` in the service's environment to choose it; the
zone's abbreviation is always printed, or its offset as `UTC-04:00` when the
abbreviation is unusable, and UTC is used when the local time cannot be read.
When the prompt is typed 120 seconds or more after that time, as after a bridge
outage or behind a busy agent, the line also says how much later, as
`Sent 2026.10.07:08:45 EDT, delivered 1 h 12 min later.`, so an agent handed
several held messages at once can tell how old each one is. The rest of that
line says that the request arrived through the chat bridge. After it, each
request prompt gives the message's `Source:` ID, its `Sender:`, and its
`Thread:` ID, and says whether the message starts a new thread or replies in an
existing one. When a provider payload with schema `google.chat.message.v1`
quotes an earlier message, a `Quoted message:` line names that message and its
text follows with every line prefixed by `> `, or an empty line by `>` alone. A
long quote keeps whole lines from its beginning and its end, with one line
`...N chars elided...` between them, where N is the number of characters left
out (`...1 char elided...` when N is 1); only a line too long to show whole is
cut inside. The `Quoted message:` line then states how many characters the
provider's quoted text has. N counts characters of the quote after each CR LF
pair has become one line break and white space at both ends has been trimmed;
the `Quoted message:` count is of the provider's text before those changes.
Identifiers stay on one line and control characters are replaced, so this text
cannot break a later terminal capture. In identifiers and quoted text, the `<`
of a reply-marker token such as `<CHAT_REPLY_` is printed as `‹`, and each
character of a run of three or more backticks or tildes as `ˋ` or `˜`, so no row
can form a reply marker or open a code fence however the terminal wraps a long
line. These tokens are found as if every non-ASCII character and every tab were
absent, because a terminal program may drop a character it draws with no width.
The look-alikes count as absent too, so when replacing one token joins its
neighbours into another, as in `<<CHAT_REPLY_`, that one is replaced as well,
and no marker or fence forms whichever of these characters a terminal drops. The
message's own text follows the front matter as sent. For a reply in an existing
thread, the prompt also prints the exact command that shows the thread's earlier
messages. The command is left out when a word of it cannot be printed on one
line or holds such a token, because a rewritten word would name a different
thread or state directory. The command starts with the absolute path of the
service's own executable, because a service need not have `agentctl` on `PATH`:

```sh
/opt/agentctl/bin/agentctl chat thread \
  --bridge-state /home/me/.local/state/agentctl/project-chat \
  --thread spaces/example/threads/one --last 10
```

If that executable's file has been deleted, the command names the file now at
the same path, normally the build that replaced it; with no file there, it
starts with plain `agentctl`.

`thread` prints the thread's retained requests and the replies captured for
them, oldest first, each with its UTC time, its age, and whether a reply was
sent. It drops the line breaks at the end of each request or reply, prefixes
every line of message text with `> `, or an empty line with `>` alone, and
rewrites reply-marker tokens and fence runs the same way as the prompt. Times
come from the bridge host's clock, and the entries are ordered by them. `--last`
selects how many of the most recent messages to show (default 10, at most 100).
The bridge retains only requests it admitted from allowed senders that have not
been retired, so retired requests, other senders' messages, and anything the
bridge never received are absent; the provider's own thread is the complete
record. Like `inspect`, it reads under the shared state lock and never writes
state or contacts Herdr, a helper, or a provider.

`sent` lists the replies the bridge holds across all retained requests, or one
request's with `--request KEY`, newest first, so an agent can check what reached
the chat:

```sh
agentctl chat sent --bridge-state /home/me/.local/state/agentctl/project-chat --last 10
```

Each entry gives the time the reply was sent, or captured while it is unsent,
in UTC with its age (a record kept before send times were recorded shows its
capture time and says that its send time is unknown; entries are ordered by
these times); its number within its request; the request key; the chat
message and thread it answers; and its outcome: `sent as` the provider's message
ID once the provider accepted it, `captured, not yet sent`, or `send in progress
or outcome unknown`, which the service retries with the same operation ID so it
is never posted twice. The first 300 characters of the reply follow, quoted as
`thread` quotes them. `--last` selects how many (default 10, at most 100), and
`--json` prints one document, schema `agentctl-chat-sent/v1`, with the same
fields, times as Unix milliseconds, `retained` (how many the bridge holds), and
each reply's whole text. Like `thread`, it holds only replies of requests that
are not retired, reads under the shared state lock, and never writes state or
contacts Herdr, a helper, or a provider.

A reply printed between its two marker lines reaches the user only if the
service reads it from the agent's screen, and a terminal agent can redraw or
clear its screen before that read. With `chat run --offer-reply-command`, each
prompt of an outbound-enabled bridge also offers a way that does not depend on
the screen: write the reply to a file and run the exact command the prompt
prints, which starts with the absolute path of the service's own executable:

```sh
/opt/agentctl/bin/agentctl chat reply \
  --bridge-state /home/me/.local/state/agentctl/project-chat \
  --request 64_LOWERCASE_HEX_CHARACTERS --reply-id 001 --file PATH_TO_YOUR_REPLY
```

When no file is left at the executable's path, or a word of that command, that
path included, cannot be printed safely on one line, the prompt gives only the
two marker lines. The prompt names the two marker lines as the normal way to
reply, since that text also stays in the agent's terminal, where the owner may
be reading, and the command only for a reply that must go before the agent's
turn ends or one written between the lines that did not reach the chat: one
reported as not sent, or one that `chat sent` does not list, as when the screen
was redrawn before the service read it. It
asks the agent to send each reply one way only, and to print a reply between
the two lines at the end of its turn if the command fails twice for it.

`reply` stores the file's text as a reply of the request, as if the service had
read it between the two marker lines of `--reply-id`, and the service sends it
like any reply it reads. The same rules apply. The ID must be one that the
request's prompt gives: the request's reply alias, or `<nonce>_<ordinal>` with
any valid ordinal. Neither the key nor the ID is a secret; both must match so
that a reply meant for another request is refused. The file is read with a
30,000-byte bound, the line breaks at its end are dropped, and the text must be
nonempty, hold no terminal control characters, fit in 30,000 UTF-8 bytes once
the agent label is added, and have no line that is a reply marker line. A closed
or retired request takes no more replies. A text the request already holds,
read from the screen or stored by an earlier command, is not stored again, so
running the command twice sends that text once. The command prints one JSON
object with `request`, `reply_id`, `outcome` (`stored` or `already_stored`),
`ordinal`, `phase` and `service_woken`, and exits 0. A refused reply exits 1
with the reason on standard error and stores nothing; that includes an empty or
blank text and a file over 30,000 bytes. A state that cannot be read or written
also exits 1, and so does a result that cannot be printed after the reply was
stored. Exit 75 means nothing was stored and the same command can succeed later.
A usage error exits 2, and so does a file that cannot be read or is not UTF-8.
The command never contacts Herdr, a helper, or a provider. It is safe while
`chat run` runs, because it opens the state without the recovery that
`chat run`, `chat tick` and `chat close` perform when they open it, and it holds
off SIGHUP, SIGINT, SIGQUIT and SIGTERM while it stores the reply.

A service running with `--offer-reply-command` listens on a datagram socket,
`.wake.sock` in its state directory. After it stores a reply that is not sent
yet, `reply` sends the request key to that socket, and the service sends the
reply at once instead of at its next reconciliation. `service_woken` says
whether the key was sent to a socket of the current user at that path; nothing
confirms that a service received it. Without a listening service, the reply is
sent at the next reconciliation, when the agent goes idle, or when the service
next starts. A socket address holds at most 107 bytes of path, and the service
binds the socket by the same absolute path that the prompt's command names, so
a service whose state directory has an absolute path longer than 96 bytes, not
counting a trailing slash, cannot listen: it logs why and runs on without the
socket, as does a service that finds something other than a socket of its own
user at that path. The prompt offers the command only with this option, because
the agent must be able to run the service's executable and write the state
directory. Like `--ignore-text-prefix`, the option applies only to the run it
is given to.

An owner or operator can explicitly publish a new root message through the
configured outbound helper without pretending it is a reply:

```sh
agentctl chat publish \
  --bridge-state /home/me/.local/state/agentctl/project-chat \
  --channel-id spaces/example \
  --request-id 123e4567-e89b-42d3-a456-426614174000 \
  'Please reply to this bridge test.'
```

`publish` accepts `--file PATH` instead of positional text. It requires an
outbound-enabled state and an exact channel from `channel_ids`, validates a
nonempty body of at most 30,000 UTF-8 bytes, and sends `thread_id: null`. The
lowercase RFC 4122 version-4 UUID is caller-owned; an uncertain result may be
retried only with the identical UUID, channel, and body. The command runs the
helper only through the reviewed process supervisor, binds the returned message
resource to the requested channel, and prints the exact v1 success receipt. It
neither writes bridge state nor creates an event-loop reply route; this is an
explicit operator action, not automatic subscription behavior.

Explicitly close reply capture for an old request:

```sh
agentctl chat close \
  --bridge-state /home/me/.local/state/agentctl/project-chat \
  --request 64_LOWERCASE_HEX_CHARACTERS
```

Closure is the provider-neutral terminal route lifecycle; idle/done status and
the first reply do not close a route because a request may send later progress
updates. A closed request remains active until its prompt delivery is confirmed,
its reaction is disabled or confirmed, and every captured reply is sent. It is
also retained while it belongs to the current inclusive replay cursor. Once all
conditions hold and a later cursor is durable, the bridge fsyncs a retirement
journal containing the exact reaction and reply operation/provider receipts,
installs a compact replay and closed-nonce guard, and only then deletes the
active request/reply files. Startup completes an interrupted retirement, as does
the next acknowledgement, reply send, closing of a request's replies, or
recording of a prompt as typed. With outbound replies enabled, so do the next
recovery scan of the pane and the next reload of the reply routes (when the
service starts, at each tick, and after each recovery pass).

The active population remains capped at 2,048 requests. The compact route and
replay ring retains the latest 4,096 retired identities, matching the pane
marker bound, and the parallel retirement audit ring has 4,096 slots of at most
256 KiB each. Older replay safety relies on the subscription contract's
inclusive monotone cursor; a cursor regression or a reused message identity
with different content fails closed. This is bounded local retention, not a
claim of infinite local audit history.

Reply markers count only on rows that can be the agent's own output. A terminal
agent draws each prompt it receives, and the output of each tool call it makes,
as an item of its own, and the rows of those items are skipped, so a reply block
that another agent quotes in a message to this one, or that a tool prints from a
file, is not posted, except in a read of an idle Claude Code pane longer than
its screen, as described below. It is not reported either, unless the first row
of that item is out of view, the item is a Claude Code prompt whose first row is
the first nonblank row of the read, or the read is such a longer read, as
described below. A prompt starts at a row whose text after its indentation is
`❯`, `›`, `↳` or `»`, alone or followed by a space or a no-break space, and its
wrapped rows continue two or more columns further right. Claude Code draws a
prompt and its input box after `❯`; Codex draws a prompt after `›`, a prompt it
holds until a running tool call ends and a hook's notice after `↳`, and its
input box after `»`. Tool output starts at a row whose text after its
indentation begins with `⎿`, or is `└` alone or followed by a space, and its
later rows continue right of that character. Codex draws `└` alone when the
output's first line is empty. A code fence in tool output neither opens nor
closes a fence.

Inside an open block these characters are reply text, such as a tree drawn with
`└`, except on a row that starts a new item: a row whose text starts with a
prompt character or a bullet (`•`, `⏺` or `●`) left of the column where the
block's opening marker starts, or with `⎿` at or left of that column. Claude
Code's compact view draws a tool call as a row at the margin of the message,
with no bullet, and its output after `⎿` at the same column. The block then ends
unfinished and is handled as a partial block, as described below, unless the new
item is a bullet row that holds the block's closing marker. Each request prompt
therefore asks the agent to put the opening line at the beginning of its
response and to make no tool call between the two marker lines. The prompt
writes both marker lines out whole. Its copy on the agent's screen is a prompt
echo, and its marker lines do not count while the echo's first row is read as a
prompt row. That row is not read so once it is out of view, or when it is a `❯`
row at the top of a capture, which is skipped as Claude Code's pinned copy, as
described below. The closing marker line stands alone on a row only in a pane
exactly as wide as that line plus the echo's indent: 19 columns for a
three-digit ID. There, if the echo's first row is not read as a prompt row, the
instruction's text before that line can be reported as an unopened block or a
remnant, and a joined read can post it.

These rules read how a row starts, not who wrote it, so they can fail both ways.
A row of the agent's own text outside a block that starts like a prompt hides
the rows two or more columns right of it, and one that starts like tool output
hides the rows right of its first character, so a block in those rows is not
read. A tool call that the compact view draws as a single row, with no `⎿` row
under it, reads as text, so inside a block it is posted as part of the reply.
An item that starts with none of these characters can show text the agent did
not write: Codex's `Goal active` notice shows the goal the agent was given,
wrapped to the left edge, and a reply block quoted in that goal is read as the
agent's own.

The bridge reads only the rows on the screen of a pane that herdr reports keeps
no scrollback, one whose `scroll.max_offset_from_bottom` is 0, such as a Claude
Code pane, for the reason given in the next paragraph. For any other pane, such
as a Codex pane, it asks herdr for the newest 4,000 lines with each wrapped line
joined into one, as herdr reads the pane for an output event; herdr returns at
most 1,000 lines for either read. If that read returns no text, the bridge asks
for the rows as the terminal draws them instead. It looks the pane up again
before each read, so a pane that starts or stops keeping scrollback between the
lookup and the read, as a program does when it enters or leaves the terminal's
alternate screen, is read the old way once, and for a pane that has just entered
the alternate screen, that one read can be the joined read described below. A
`scroll` value that is not an object of those three counts fails every lookup
that reads that pane, for prompt delivery as well as for reads, rather than let
the bridge guess. A lookup by session reads every pane, so one pane's malformed
value fails every lookup by session. The rows above the first nonblank row that
starts at the left edge continue an item whose first row is out of view, which
can be a prompt or tool output, so an opening marker there opens no block,
although an unavailable ID in it is still reported, and a code fence that opens
there ends at that row. Once a prompt has scrolled off the top of the screen,
Claude Code can draw a copy of it over the screen's top row, at the left edge
and cut to one row, above rows of whatever item the screen starts in. So a `❯`
row at the left edge that is the first nonblank row of a capture is skipped: it
starts no prompt, and it is not the first row at the left edge. A Claude Code
prompt that really starts at the top of a capture is then read as rows above the
first row at the left edge, so a block it quotes is not posted, but it is
reported unless its text is the end of a stored reply. Codex draws no such copy,
so a Codex prompt row at the top of a capture starts a prompt, as it does
anywhere else. A block is therefore posted only by a capture that shows both the
first row of the message that holds it and the whole block. A block that starts
its message needs only the whole block in view; a block after other text in its
message also needs the message's first row, which leaves the screen sooner. So
from a Claude Code pane a block is not posted if it is taller than the screen,
or if its closing marker is more than a screen below the first row of its
message, as in a long message that ends with the block; a recovery scan that
sees its closing marker reports it instead, as described below. A capture that
sees only part of a block handles that part as a partial block, as described
below, except in the longer reads described next.

A Claude Code pane keeps no scrollback, yet a read of more lines than its screen
has rows can return more. Herdr 0.8.0 builds such a read of an agent that is
idle and takes mouse wheel input: it scrolls the agent's view up with wheel
events, joins the screens it sees where they overlap, and scrolls the view back
down. That can take up to 20 seconds, and anyone watching the pane sees the view
move. The join keeps each row that differs from the row at the same position on
the screen before, so each time the copy of a prompt that Claude Code draws over
the top row changes, the copy lands between rows of the conversation. That is
why the bridge reads only the screen of a pane that keeps no scrollback. Herdr's
output subscriptions never scroll the view, so an output event never carries
such a read, but the bridge's own reads of a pane still get one when herdr does
not report whether the pane keeps scrollback, and can get one once when the pane
enters the alternate screen between the bridge's lookup and its read. The extra
rows come before the screen's rows, so in every such read they decide how the
screen's top rows are read, and the screen's own pinned row is no longer the
first row of the capture: it starts a prompt, which hides the agent's rows under
it. These reads can mislead the rules above both ways. Rows of a prompt whose
first row is out of view can follow a row of the agent's message, so a block
quoted in that prompt is posted as the agent's reply. The agent's own rows can
follow a pinned prompt row, a stale prompt row, or a tool output row. Where they
are indented far enough to read as that item's rows, two or more columns right
of the prompt character or right of the `⎿` or `└`, a block in them is skipped,
neither posted nor reported, and a block of the agent's that such a row lands
inside is reported as having no closing marker, although the agent wrote one. A
later read that shows only the screen reads the block by the rules above, if it
is still in view: it is posted if that read also shows the first row of its
message, and otherwise reported, unless it was already reported.

Reading only the screen has a cost. For a Claude Code pane, the bridge's reads
when it starts, at each reconciliation, when the pane settles idle or done, and
after an output event names an unknown reply ID see only the screen. An output
event carries herdr's read of the pane taken when the matched line appears,
which for such a pane is its screen, so a block that fits on the screen is
normally caught when its closing marker appears. If that event is missed, for
example while the bridge waits to subscribe again after losing its connection to
herdr, or while it restarts, and the block leaves the screen before the bridge's
next read, the block is neither posted nor reported. A joined read could have
recovered it. Before herdr 0.8.0 added those reads, a read of such a pane
returned only its screen, so the bridge lost such a block then too. The bridge
accepts that loss rather than scan rows joined from different times, which can
post a block the agent did not write. While the bridge counts the agent as
working, as described below, it also reads the pane every 2 seconds, so then a
block is lost this way only if it appears and leaves the screen between two of
those reads, while those reads fail, or while the bridge restarts. Two of those
reads can be further apart than 2 seconds, because none is made while the
bridge types a prompt, which can take up to about 2 minutes for each prompt
whose paste is slow to show. A block that the screen cannot show whole
together with the first row of its message, as described above, can be lost even
when no event is missed: only recovery scans report a partial block, and during
a busy turn a recovery scan normally comes only when the pane settles, so if the
agent writes more than a screen after such a block before then, the block is
neither posted nor reported.

Each request's prompt gives one reply ID, a short number, for every reply to
that request: `001`, `002`, and so on, counting across all requests and past
`999` with more digits. The prompt asks the agent to use the same two marker
lines, and so the same ID, for every reply. The bridge assigns a number when it first writes
a prompt that shows it, and never assigns it again: the next number is kept in
`reply-aliases.json` in the state directory and saved before the prompt is
written, so a restart, a crash, or a rollback to an earlier release and back
does not reuse one. A number that the agent writes in a marker line before the
bridge assigns it, such as a guessed or example ID, is reported like any other
unavailable ID and skipped when assignment reaches it; up to 1,024 such numbers
are kept, the lowest ones. The number of a closed request stays recognized as
long as that request's long ID does, so a late block under it is ignored rather
than reported. An earlier release reports a short ID as belonging to no request.

Just before it writes a prompt that gives a request its number, the bridge reads
the agent's pane, as described above, and skips every number that the read shows
written as a short reply ID anywhere: in a block, in running text, or in a
prompt or tool output the agent received. These count toward the 1,024 skipped
numbers kept for later requests, and the request whose prompt follows the read
gets none of them, however many the read shows. So a block already in the pane
when a request gets its number is never posted as that request's reply; where a
capture reads it, it is reported as naming no open request. If the pane cannot
be read, the prompt is not written and the request waits for the bridge's next
attempt. A block that the read does not show can still be posted to a request
that gets its number afterwards, if a later capture sees it: one that comes back
into view, as when someone scrolls a Claude Code pane's view up, and one in an
output event that herdr read before the block left the view but the bridge
handles only after the prompt. Once a request's prompt has reached the agent, a
block under its number is posted to it, even if the agent meant it for another
request, as a block under a long ID is.

A request gets its number when its prompt is written, but the prompt can then
wait in the agent's queue, for example while the agent is busy, and the agent
cannot know the number before the prompt is typed. A block under that number
before then was written for another request, by an agent that guessed the next
number or copied an example. While the queue holds the prompt in its inbox, or
does not hold it at all, the bridge posts no block it reads under that number
and remembers the text of each complete one in `reply-aliases.json`, so no block
with that text is posted to the request after its prompt is typed either. That
text is compared as a stored reply's is, as described below, so a paragraph the
terminal re-wraps or a border it redraws still matches, but a table redrawn in
one of the ways listed there can read as new text. A block with new text under
the number is posted then. Up to 256 remembered blocks are kept, the newest.
Each delivery, of a request's prompt or of a routing-error prompt, can type
every prompt waiting in the queue, so before it types any, if a request whose
prompt has not been typed has a number, the bridge reads the pane and remembers
in the same way the blocks the read shows under such numbers. It reads the pane
too while `reply-aliases.json` cannot be read, but then remembers nothing, as
described below. If the pane cannot be read then, or a block it shows cannot be
written to `reply-aliases.json`, the bridge types nothing and tries again later.
When a recovery scan reads a block that is not posted for this reason, the agent
gets a routing-error prompt that says the block was not sent, why, and that no
block with its text will be sent to that request, and lists the open requests.
It names the request the block was probably meant for in the same case as for a
block under an unavailable ID, described below. While a request's queue entry
cannot be read, a block under its number is held: it is not posted, remembered,
or reported, and a later capture decides.

These rules see only the bridge's own queue, and they have gaps. A prompt typed
into a Claude Code session that is still working waits in that program's own
queue, which the bridge cannot see, so a block under its number written then is
posted. A block that first appears after the read before typing is posted unless
a capture reads it before the prompt is typed, and a nonzero `--ready-timeout`
widens that window, because the drain then waits for the agent before it types.
A block written while the prompt waits that no capture reads, and that is out of
view at the read before typing, is posted if a later capture sees it: one that
comes back into view, as when someone scrolls a Claude Code pane's view up, and
one in an output event that herdr read before the block left the view but the
bridge handles only after the prompt is typed. A remembered table that the
terminal redraws after the prompt is typed, in one of the ways listed below, can
read as new text and is then posted. Each rendering of a table whose cells wrap
differently is also reported as a block of its own, so it can bring the agent
one more routing-error prompt. Other commands that type queued prompts do so
without that read: `agentctl send`, `agentctl drain`, and `agentctl goal` given
a goal, and the same three commands of the older `herdr-agent`. A request whose
queue entry shows that typing its prompt started counts as reached, even if the
typing then failed. While a request's queue entry cannot be read, the read
before typing does not remember blocks under its number, so if its prompt was in
fact waiting and is typed, a block under its number that is still in view once
the entry can be read again is posted. A remembered block is forgotten once 256
newer ones are remembered. While `reply-aliases.json` cannot be read, nothing is
remembered, so once the file is repaired, a block still in view under the number
of a request whose prompt was typed in that time is posted to it. Only a
recovery scan reports such a block: one that the read before typing or the
capture after an output event remembers is reported only if a recovery scan
reads it too.

A request prompted before short reply IDs, and every request prompted while
`reply-aliases.json` cannot be read, gets a long reply ID instead: the request's
nonce, an underscore, and a number from 1 to 999999 with no leading zero, which
the agent increments for each reply. Long IDs stay accepted for every request.
While the file cannot be read, the service logs that it is unusable, `chat tick`
reports that among its notes, and `chat status` shows it in
`reply_alias_problem`, which is otherwise null; blocks under short IDs are
reported as naming no open chat request, and the file is left as it is for an
operator to repair. Deleting it starts the numbering again at `001`, as a new
state directory for the same pane does, and replacing it with an older copy
starts it again at that copy's next number. The read before each prompt still
skips the numbers the pane shows, but a block under an old number that is out of
view then can reach the new request.

Herdr reports an output pattern only when it starts to match, and a request
keeps its short ID for every reply, so while the closing line of one reply is in
view, the closing line of the next raises no event. While a closing line under
an open request's short ID is in view, the service therefore reads the pane
itself every 2 seconds, until no such line is left. In view means in the
bridge's read, which is herdr's window for output events too: the screen of a
Claude Code pane, and the newest 1,000 lines of a Codex pane. Each of these
reads takes the lock that prompt delivery takes, looks the pane up in herdr, and
reads it, up to 1,000 lines of a Codex pane, but saves no snapshot. A capture of
the read follows for each open request whose closing line it shows, as for an
output event, and reads that request's queue entry too when the bridge has not
seen its prompt typed and the request has a number or `reply-aliases.json`
cannot be read. A closing line inside a prompt or tool output the agent received
keeps them going too, although it is never posted. A read that fails is logged
once and retried every 2 seconds, and its first success after that is logged as
well.

A turn can also write a reply block and then push it off the screen with more
output before it ends. If another line keeps the pattern matched without
starting the reads above, such as a closing line under a long reply ID inside a
prompt the agent received, the block's closing line raises no event, and the
read when the pane settles comes too late. So while outbound replies are on and
some request is open, the service also reads the pane in the same way every 2
seconds for as long as it counts the agent as working. A request stays open
until `chat close`, so in practice these reads run whenever the agent works, and
each costs what the reads above cost. The service counts the agent as working
while any of these holds:

- herdr reports its status as `working`;
- the last of the service's own reads of the pane that could tell showed a
  running turn, or the rows of an output event since that read showed one;
- the service has asked for a request prompt or a routing-error prompt to be
  typed, even if the typing failed, and none of its own reads that could tell
  has shown since that no turn runs.

The service's own reads are the ones this guide describes, other than those it
makes to type a prompt. The rows of an output event can show a running turn but
cannot end one, because herdr may have read them before a prompt that the
service typed while the event waited to be handled. Herdr's status alone is not
enough, because herdr's status rules can report a Claude Code pane that is
running a turn as idle. The service looks herdr's status up at each
reconciliation, after each wait for an output event, and at each scan of the
request records that finds prompts waiting to be typed, as described under
Prompt delivery below. Such a wait ends when
herdr reports a matched line or a change of the agent's status to `working`,
`idle`, or `done`, when a read, a reconciliation, a scan of the request
records, or the retry of a failed lookup is due, or when a provider notice or
a signal arrives. While no output
subscription works, the herdr status by which the service counts the agent as
working comes only from the lookups at each reconciliation and at such scans;
the lookups that it makes to subscribe, to read the pane, or to type a prompt do
not change that
status. No event depends on a lookup after a wait that ended with no event, so
if that lookup fails, the service ends its next wait within 2 seconds to try
the lookup again, and does the same after each further failure, until a lookup
after a wait succeeds. If the subscription ends first, because that wait fails
or the reply routes change, the lookup after the first wait on the next
subscription is the retry. Until a lookup after a wait succeeds, the service
counts the agent as working by herdr's status only if the last lookup that
succeeded, at a reconciliation, at a scan, or after a wait, reported `working`. The service
logs the first failure and that success. A failed lookup after an event, or at
a reconciliation, stops `run`. The first wait on a new output subscription ends
at once, so that its lookup shows an agent that was already working when the
service subscribed, which herdr raises no event for.

A screen shows a running turn when one of its bottom 16 rows, counted up from
the last row with text, holds `esc to interrupt`, as Claude Code's status row
and Codex's progress row do during a turn, or one of the hints both show while
a message waits in their queue. Only the text is matched, so a message of the
agent's that quotes one of them in those rows counts too, and keeps the reads
going until later output moves it out of those rows. For a while after a paste,
Claude Code shows `paste again to expand` in place of that status row, whether
or not a turn runs, so a screen whose last row with text holds that hint, and
whose bottom rows hold neither marker, cannot tell. Any other screen shows no
running turn, even if a turn has just started and its status row is not drawn
yet; the reads then go on only if herdr reports the agent as working.

Two more reads are made at once in the same way, whether or not a request is
open, unless a failed read is waiting for its retry. One comes before each pass
that handles queued requests, whether new ones from the chat, ones an earlier
pass left for later, or ones a scan of the request records found waiting to be
typed, and before the rescan the service makes when new requests
arrive faster than it can queue them: typing their prompts, and the turn that
starts, can push a block of the turn before off the screen before herdr reports
that turn's end. The other comes when herdr reports that the pane settled idle
or done but the pane has already left that status, as when a prompt typed
meanwhile has started another turn.

Replies are recognized by their text, not by the number in their reply ID. When
a block appears under any reply ID of an open request, the bridge compares its
text with the replies it has already stored for that request. The comparison
ignores whitespace and the box-drawing characters, U+2500 to U+257F, that a
terminal draws for table borders, so a paragraph the terminal re-wraps or a
border it redraws at another width still matches. Every other character counts,
including the block elements of a progress bar. A block whose text matches a
stored reply is not posted again; any other block is stored as the request's
next reply and posted. So an answer the agent sends twice to one request, under
one ID or under two, is posted once, and two identical short replies to one
request, such as two `ok` progress notes, are posted as one. The same text sent
to two requests is posted to each.

A table cell that wraps onto several lines at one width and fits on one line at
another changes the order of the characters, so tables are also compared column
by column. A row line starts, ends, and divides its cells with `│`; a table
drawn with other characters, such as `|` or `┃`, is always read in row order.
Adjacent row lines are read column by column, as one row, only when a
word-wrapping renderer could have drawn them from one row: every line has the
same cell widths and a space at each side of each cell; each column's text sits
at the top of the lines or is centred in them, with the odd line below; the
text of at least one column fills the lines; and each line break was needed
because the next word would not have fit. A renderer that keeps the space of a
line break, such as Claude Code's, starts the line after an exactly full one
with that space, or puts the space on a line of its own when the next word
fills a whole line, and the space counts toward its line. Widths are counted in
terminal columns, as a renderer pads cells: an emoji or a CJK character takes
two columns, and a nonspacing mark such as a combining accent none. That match
counts only when the stored reply's tables have other cell widths, since at the
same widths a renderer wraps the same text the same way. The block is then
skipped and logged, as described below. It is logged because one case reads the
same without being a redraw: a table with no rule between its rows, drawn at
other widths, whose rows all pass those tests and whose text differs from a
stored reply only in how it divides into rows or where a space falls inside a
cell.

A table redrawn at another width can still be posted again. Adjacent row lines
with the same number of cells are read as one group, which ends at any other
line, such as a rule. A group that fails the tests above is read in row order,
so a redraw that wraps it differently reads as new text. That can happen in
four cases; a renderer that lays out tables unlike Claude Code, such as by
centring text with the odd line above or drawing cells without a space at each
side, can cause others:

- a table with no rule between its rows, where one group holds several rows.
  Claude Code draws a rule between every two rows, so this needs another
  renderer;
- a cell holding a character that the agent's renderer measures at another
  width than the bridge does, such as some emoji sequences and most spacing
  vowel signs of Indic scripts;
- a cell holding a run of two or more spaces, or words joined by a no-break
  space or a tab. A renderer can leave the extra spaces at the end of a line,
  where they look like padding, and need not break a line at a no-break space
  or a tab, so the bridge cannot tell that a break was needed;
- a redraw that switches between a grid and one `Header: value` line per cell,
  which a renderer can draw when a cell would need too many lines or the grid
  would not fit the terminal.

A block that cannot be posted is skipped, and the other blocks are still
captured. That covers a block that is empty, holds terminal control characters,
or exceeds 30,000 UTF-8 bytes once the agent label is added, a block beyond the
request's reply limits, and a block that matches a stored reply only column by
column. `chat run` logs such a block as
`TIME agentctl: chat reply capture: reply block ID (text HASH) was not sent: REASON`,
where TIME is the UTC time that begins every `chat run` log line and HASH is the
first 12 hex digits of the SHA-256 of the block's text without whitespace or
box-drawing characters. A process writes each distinct line once while it
remembers it; it remembers the latest 4,096 distinct lines, and writes a
forgotten one again. One capture pass logs at most 128 such lines, so on a
screen with more refused blocks than that, the rest are not logged while they
stay visible. The agent is not told. Nothing the agent prints stops the bridge:
a snapshot larger than 2 MiB is read from its newest complete lines, with one
log line, and a snapshot with more than 4,096 reply blocks is read from its
newest 4,096, with one log line from a recovery scan. Only state, file system,
and provider faults are errors.

A reply marker in the pane whose ID is not available, such as a typo or a stale
block left in scrollback by another bridge state, produces one routing-error
prompt to the agent. An ID is unavailable when its number is malformed or it
names no request the bridge knows. A well-formed ID of a closed request is
ignored. The prompt is never posted to chat. It says that the block marked with
the unavailable ID was not sent, and then lists the open requests, each by the
ID of its next reply, as they stood when the prompt was written. First come
those with no reply yet, most recent first, each with the time since the bridge
admitted it, such as `10m ago`, because a mistyped ID was most likely meant for
one of those. Then come those that already have a reply. The first list names up
to 32 requests and the second up to 10, and each counts the rest, as in
`... (+37 more)`. When every listed request has a reply, or none is listed, the
prompt says so. It ends by asking the agent to send the block again with the ID
of the request it answers. An open request is listed once delivery of its prompt
to the agent is confirmed or uncertain, and before that while the agent's queue
holds the prompt, as it does when a submission failed after queuing it and waits
for a retry, since that prompt reaches the agent through the same queue as the
routing-error prompt. A request whose prompt has not reached the queue, because
the bridge has not yet submitted it or its submission was cancelled or failed
before queuing, is not listed. Only a prompt that reports an unavailable ID
looks for request prompts in the queue. A request whose record there cannot be
read is listed too, since its prompt may have reached the agent, and the
routing-error prompt is sent all the same.

The prompt says that the block was probably meant for a request when that
request is the only listed one with no reply yet that the agent may have seen
when it wrote the block, and its prompt is known to have left the queue's inbox,
where a prompt waits until the queue types it into the pane: its delivery is
confirmed or uncertain, or the queue reports it past the inbox. A request whose
prompt still waits in the inbox is listed but does not count, since the agent
had not seen it when it wrote the block. A request whose queue record cannot be
read counts, since the agent may have seen it, but is never named, so while one
with no reply yet is listed, no request is named. The bridge cannot tell when
the block was written, so a request whose prompt left the inbox after the agent
wrote the block, but before the bridge reported it, can still be named. The
exception is a request whose prompt reaches the agent when the bridge delivers
an earlier routing-error prompt still waiting in the queue, which it does before
writing a new one: the open requests are taken before that delivery, so such a
request is listed but not named. When the prompt names a request, it also asks
the agent not to send a block it did not write, such as one quoted in a message
it received, since the agent could otherwise send such a block under that
request's ID.

A recovery scan also reports a partial block of an open request: an opening
marker with no closing one after it, or an unopened block, which ends in a
closing marker in a message that shows no opening one before it or whose first
row is out of view. It does not report a part that holds no text or is the start
or the end of a reply already stored for that request, read in row order or
column by column. A part cut inside a table row that wrapped at another width
than the stored reply's is still reported, once. The prompt says what the
capture was missing: the start of the message that holds the block or an opening
line for the block, or the block's closing line. A block that a prompt or a tool
call interrupts reads as both: the rows before the new item as a block with no
closing line, and the rows after it, up to the closing line, as an unopened
block. The prompt then asks the agent to send the block again, starting a
message with its opening line and writing the whole block in that message, with
no tool call inside it, and not to send a block it did not write, such as one
quoted in a message it received. A block another agent quoted in a message whose
first row is out of view, including a Claude Code prompt under the copy of its
first row, or in a Claude Code prompt whose first row is the first nonblank row
of the read, and a block a tool printed in output whose first row is out of
view, are reported too, as described above. So, in a read of an idle Claude Code
pane longer than its screen, are a quoted block and the agent's own block with a
copied prompt row or a tool-output row inside it, which reads as a block with no
closing line. When a closing line was missing, it first says that a block still
being written goes out once it is complete, if the screen then shows the first
row of its message and its closing line, since the agent may not have finished
the block when the pane was read. A complete block under that ID is still posted
if its text is new. One prompt can report both unavailable IDs and partial
blocks.

A partial block is identified by its ID, by which end of it was missing, and by
a digest of 64 characters of it, not counting whitespace or box-drawing
characters: the last 64 of an unopened block, and the first 64 of a block with
no closing line. A block with fewer than 64 such characters is identified by all
of them. Those characters stay the same while the block scrolls, so a block
leaving the top of the screen is not reported again while at least 64 characters
of it are in view, and what is left of it after that is not reported if an
unopened block under its ID was already reported. The rows of an unopened block
begin no higher than the last row above its closing line that starts at the left
edge with a bullet (`•`, `⏺` or `●`) and a space, as the first row of a message
does, so while that row is in view, rows of an earlier item are not counted,
even once the prompt or tool output row that started that item has left the
screen or reads as Claude Code's copy of a prompt. Two different blocks under
one ID are each reported. A partial block that only scrolls up the screen is
therefore reported once. It can be reported again when a later recovery scan
shows it differently, such as a block with no closing line and fewer than 64
such characters that has grown; a block reported with no closing line while it
was still being written, which is reported as an unopened block once a recovery
scan sees it complete but not the first row of its message; and a block first
reported when fewer than 64 such characters of it were in view, at the top of
the capture, once a recovery scan shows the first row of its message or 64 such
characters of it, as it can when rows move back down the screen.

A recovery scan also reports a reply block of an open request whose opening or
closing tag shared its row with other text. A marker counts only alone on its
row, so such a tag neither opens nor closes a block, and the reply it was meant
to mark is not sent. Two shapes are reported. In the first, while no block is
open, an opening tag shares its row with other text, such as
`<CHAT_REPLY_001>Done.`, and a closing tag of the same ID follows it, on that
row or a later one, alone on its row or not, as in
`Reply: <CHAT_REPLY_001>Done.</CHAT_REPLY_001>`. In the second, a block that an
opening marker line began holds a closing tag of its ID in a row with other
text, such as `Done.</CHAT_REPLY_001>`, and ends with no closing marker line. A
single tag in a row with other text is not reported, since that is how prose or
a tool call quotes a tag. An opening tag is read for its closing tag until a
prompt row, a tool output row, a bullet row that starts left of the text of the
tag's row or at the left edge, as a new message does, unless that row is a
closing marker line of its ID, another marker line, or a newer opening tag in a
row with other text. One with no closing tag in view by then is not reported, so
a block still being written is reported once its closing tag is in view. A
closing tag in a block that closes is text of the reply. Nor is a tag reported
inside a fenced code block, prompt echo, or tool output, above the first row at
the left edge of the capture, where it can belong to a prompt echo such as the
request prompt's own instruction, or when its text is part of a reply already
stored for that request; the start of a stored reply is not enough, since the
block's text after the start can differ. Nor is a block reported whose tags are
in a tool call's command. A row starts what may be a call when its text, after
any bullet, is a tool name that starts with a capital letter, or an MCP tool's
name followed by `(MCP)`, and then its arguments in parentheses with nothing
after the parenthesis that closes them, such as `Bash(...)`, or when, after a
bullet, it starts with `Ran`, `Running`, `Called` or `Calling` and a space, as
Codex draws a call. A row whose parentheses close before other text, such as
`TODO(owner): ...` or `Fixed in parse_marker(): ...`, is text. The command goes
on in the rows right of that row's bullet, or of its text when it has none,
counting the parentheses each row opens and closes, and is complete when the
last of them ends with the parenthesis that closes its arguments. A parenthesis
of a further row that would close the arguments before other text in its row,
such as `1)` in a heredoc, is text of the command, and so is one at the end of a
row that another row of the command follows, such as a `case` pattern `a)`. A
further row that is a whole call by itself, such as `Read(src/lib.rs)`, closes
the parentheses it opens; when that call's own output row follows it, at or
right of its column, only that row was a command, and the tags in the rows above
it are read like any others. A later call that is not a whole call by itself,
such as `Bash(printf '%s' ')')` or a call with a heredoc, is counted with the
rows above it. Codex draws each further row of its command after `│`, and its
command is complete on any row. Text can start the same way, such as
`• Ran the checks:`, so those rows count as a command only once the call's
output row, after `⎿` or `└`, follows the complete command at or right of that
column. A further row of a call drawn with parentheses that starts right of the
first row's text, where Claude Code draws a command's further rows, is part of
the command whatever it starts with, such as a heredoc row `• item`, `❯ next` or
`└ lib.rs`, and an output row there confirms no later call. Any other prompt
row or bullet row, an output row left of that column or after an incomplete
command, any other row at or left of that column, or a further row of a Codex
command that does not start with `│` shows they were text, and their tags are
read like any others. A block whose misplaced tag a command holds is
read as it is when no such tag is read, so a closing marker line in a command,
after an opening tag in a row with other text above the command, is reported as
a block whose opening line the screen did not show. Nor is a tag of a closed
request, of no request, or of a request whose blocks the bridge holds because
its prompt may not have reached the agent; unlike a marker of such a request
alone on its row, it is not a routing error. For this reading only, an opening
tag of an open request in a row that starts at the left edge with a bullet, as
the first row of a message does, ends a code fence that an earlier message left
open, as an opening marker line after a bullet ends it for every purpose.
Reading these tags never changes which blocks are sent.

The prompt names the ID, says that a tag shared its line with other text so the
block was not sent, that each opening and closing tag must be alone on its own
line, and that a tag mentioned in other text should leave out its angle
brackets, and then asks for the block again, as it does for a partial block. It
names each such ID once, and speaks of blocks when it reports more than one such
block, even under one ID. A block reported for such a tag is not also reported
as an unopened block or as a block with no closing line, so when the prompt also
names its ID for one of those, that is another block under it. The entry such a
block would have had as one of those is kept as reported once the block is
reported, though the prompt does not name it, so the block is not reported again
once its misplaced opening tag scrolls out of view and its closing marker line
reads as the end of an unopened block, including when the pane has wrapped the
block at another width since. A block that has not been reported keeps no such
entry, and nor does a block whose rows, read without its misplaced tag, are the
start of a reply already stored, so a later block that starts the same way is
still reported. Such an entry is kept only while the history has room for it
beside the entries a prompt names or is about to name, so it does not stop a
prompt the history has room for; it does count toward the history's limit below.
It is recorded with the prompt that reports its block, before that prompt is
sent. The prompt always says that a block still being written goes out once it
is complete, since such a block can quote its own closing tag before its closing
marker line is written. Such a block is identified by its ID and by its opening
tag or marker, all the characters after it up to its closing tag, not counting
whitespace or box-drawing characters, and that tag, whichever of its tags shared
a row with other text. So such a block is reported once while it scrolls or
wraps at another width, including when a pane moves one of its tags onto a row
of its own or off one, a later reply with different text is reported again, and
an exact repeat of a reply already reported is not. Some mistakes are not
caught: an opening tag in a row with other text whose closing tag never comes
into view; tags in two messages, or with a prompt, tool call, or tool output
between them; a tag split across two rows; both tags inside a tool call's
command, or inside text that starts the way a call does when a tool output row
follows it as one follows a call, such as a remark in parentheses after a
capitalized word that closes at the end of its row, or a remark whose
parenthesis a later call closes, such as `Bash(printf '%s' ')')` or a call with
a heredoc; and a block whose marker lines are each alone on their rows, but
which an earlier message's open code fence still covers, as it does when the
block does not start its message. Some text that was not meant as a reply is
reported: a remark that names both tags of an open request with their angle
brackets, such as
``<CHAT_REPLY_001> and </CHAT_REPLY_001> each went on their own line``, once for
each distinct text; a tool call's command that holds both tags of an open
request while its output row is not yet drawn, such as a command Codex is still
running, or that is not read as a command: one whose parentheses do not balance,
such as one that quotes a `(` it does not close, or that balance only with rows
indented further than a row of it that starts with a prompt or output character,
since such rows are read as that prompt's or output's, one whose first row shows
other text after the parenthesis that closes its arguments, such as a note
Claude Code can draw there, or does not show that parenthesis, and one that
Codex wraps without `│`, once for each distinct text; a call whose row and
output row are both right of the text of a remark above it that starts the way a
call does, which Claude Code does not draw, once for each distinct text; a block
still being written that quotes its own closing tag in a row with other text,
while its closing marker line is not yet in view; and a quoted message whose
bullet is at the left edge of a code fence, which reads as a new message. A
block that is a partial block when its misplaced tag is not read takes that
partial block's place among the 4,096 blocks and partial blocks a capture reads.
Up to 4,096 other such blocks are read apart from those, so a screen full of
them cannot hide a block; a capture with more is read from its newest 4,096,
with one log line from a recovery scan.

Reported entries are kept in `fence-feedback.json` in the bridge state
directory, so each unavailable ID and each partial block entry is reported at
most once per state directory, including after a restart. An unavailable ID is
kept as the ID itself. A partial block is kept as `ID KIND HASH`: KIND is
`unopened` for an unopened block, `remnant` for fewer than 64 characters of one
at the top of the capture, `unclosed` for a block with no closing line, and
`inline` for a block whose opening or closing tag shared its row with other
text, and HASH is the first 12 hex digits of the SHA-256 of the characters its
digest covers. A history written by an earlier release keeps the bare ID of a
partial block, and that ID still covers every partial block under it. A new
state directory starts with no reported entries. A marker that stays visible
after its report is left out of later prompts, and a later block that reuses a
reported unavailable ID is not reported again. A block under an ID that belongs
to no open request is never posted. Once its ID is reported, its only trace is a
log line, and only recovery scans write that line. Recovery scans run when
`chat run` starts, at each `chat tick`, at each reconciliation while the agent
pane is idle or done, when the pane settles idle or done, and after output names
a reply ID that belongs to no request the bridge knows. Each recovery scan that
sees markers or partial blocks already reported logs one
`already reported, so not repeated` line that names up to 8 of them and counts
the rest, up to 128 per scan. Other captures do not write that line, so a reused
ID that leaves the screen before the next recovery scan leaves no trace. Already
reported entries are set aside before the bound on new ones, so a screen full of
old markers cannot hide a new one. While a routing-error prompt is still queued,
newer entries wait for it instead of producing a second prompt. The exact
pending prompt is saved before submission, so recovery settles its original
queue ID even if a crash hides the submission result or newer unavailable
markers appear. A prompt whose queue outcome is uncertain counts as reported: it
is not submitted again, even if it never reached the agent. The history retains
up to 4,096 distinct reported or pending entries; an entry the file holds more
than once is read as one, so its repeats take no room. At that limit, new
diagnostics stay held; reported entries are never evicted or submitted again.
Deleting `fence-feedback.json` clears the history, so the entries it held can be
reported once more. If the file cannot be read or is outside its bounds, the
error names it and diagnostics stay held until it is repaired or deleted.

A per-thread post-rate breaker bounds any remaining reply loop. One provider
thread may reserve 8 distinct reply operations within 60 seconds. That leaves
room for several requests in one thread, each with progress updates and a
multi-message answer. Request prompts do not state this budget, because short
progress updates are welcome.
The next reply to that thread trips the breaker. Replies to that thread then
stay captured but unsent for 300 seconds; replies to other threads are
unaffected. Held replies go out in order, under their original operation IDs, at
the first retry after the cooldown ends: the next reconciliation (every 300
seconds by default; see `--reconcile-interval`), the next time the agent pane
settles idle or done, the next captured reply for the same request, a restart of
`chat run`, or `chat tick` while the daemon is stopped. Those sends count
against a new window, so a longer backlog goes out 8 at a time, with a new hold
after each group of 8. The breaker therefore bounds only fast loops. A loop
never trips if each post starts at least 60 seconds after the receipt of the
post 8 before it, so it can post up to 480 times an hour to one thread. A faster
loop trips on its 9th post within 60 seconds; one that posts every 3 seconds
trips 24 seconds in. It then sends 8 more posts after each 300-second hold,
about 96 an hour. While a thread is held, every attempt fails with a
`post-rate breaker` error that names the thread and the release time in
milliseconds after the Unix epoch. `chat run` logs that error, and `chat tick`
reports it and exits with status 75. `chat status` reports the breaker under
`reply_breaker`: its limits (`max_replies_per_thread`, `window_seconds`, and
`cooldown_seconds`), each held thread in `held_threads` with
`held_until_millis`, `held_for_seconds`, and
`recent_or_unresolved_reservations`, and an `error` that is null unless the
record is unusable. Status never changes the record, and it lists only threads
whose trip is still live: a thread whose trip has expired is not listed even if
8 of its reservations are still unresolved, and its next new reply trips the
breaker again. Neither the agent nor the chat thread is told about a hold: a
notice to the agent could prompt more replies, and a notice in the thread would
be one more post to the thread being held.

To release held threads early, first confirm that no reply loop is running, then
delete `reply-breaker.json` from the bridge state directory. This releases every
thread and discards the budget history of all of them; held replies go out at
the next retry. If that file cannot be read or is outside its bounds, replies to
every thread stay held until it is repaired or deleted: each publish error names
the file, and `chat status` shows the failure in `reply_breaker.error`. A
reservation or trip stamped later than the current time, as after the wall clock
steps back, is moved to the current time, so its window or cooldown restarts
once instead of lasting as long as the step. The next send attempt to any thread
saves the moved stamps; until then, `chat status` reports such a trip as held
for a full cooldown from the time of each call. Reservations are durable before
a provider call, including calls whose outcome is unknown. Unresolved
reservations keep their budget until the same operation is reconciled or the
ledger is explicitly reset. A valid receipt starts its 60-second retention
window; retrying an expired completed reservation must pass the current budget
again. A full ledger holds new sends until completed entries expire. This can
conservatively hold replies after a failed attempt.

## Prompt delivery

A request's prompt reaches the agent through the agent's queue. The pass that
handles a request puts its prompt in the queue, and the queue types it only
while herdr reports the agent's pane `idle` or `done`. It refuses while herdr
reports `blocked`, and for any other status it waits up to `--ready-timeout`,
which is 0 by default, so that by default a pass does not wait for the agent.
Nothing is typed either when the pane's composer holds text that nobody
submitted, since the prompt would join that text, or when the queue cannot be
read or the read before typing fails. Such a request stays `pending`, or
`submitting` when the pass could not read what the queue did with its prompt,
and its record keeps the reason, which `chat inspect` shows in
`delivery.error`. A request whose prompt the queue may have begun to type is
`delivery_uncertain` instead, and its prompt is never typed again, because a
second copy could reach the agent.

### Typing while a Claude Code agent works

`chat run --deliver-while-working` lets the queue also type into a Claude Code
pane that herdr reports `working`, so a request reaches the agent while its
current turn runs instead of after it: Claude Code holds the prompt and runs
it when the turn ends. Off by default. Only Claude Code panes are typed into
this way; other harnesses, and prompts that start with `/`, which open a menu
rather than reach the composer, still wait for `idle` or `done`. The usual
checks still apply. The composer must be empty first: a draft still refuses
the prompt, while the faint hint Claude Code draws in an empty composer, such
as `Press up to edit queued messages`, and the cursor cell are not a draft.
The submission must be verified on the screen: the prompt leaves the composer,
and the queued-message hint, a running turn or the prompt itself shows. An
Enter that Claude Code drops, as it can while its hooks run, is pressed again
with a doubling wait for up to 60 seconds. If the prompt is still in the
composer then, the queue takes it back out with editing keys (`ctrl+e`,
`ctrl+u`, and `backspace` where those change nothing) and, once the composer
is proved empty and nothing shows the prompt was accepted, records the request
`pending` without counting an attempt, so a later retry types it again. If
the composer cannot be proved empty, the prompt is left there and the request
is `delivery_uncertain`, as for any prompt left staged. The retries every 10
seconds also run while the pane is `working`.

### Clearing a draft left in the composer

A draft left in the coordinator's composer holds every request behind it for
as long as it stays there. `chat run --stale-composer-clear-after DURATION`,
for example `--stale-composer-clear-after 30m`, clears such a draft once it
has stayed unchanged for DURATION while a request waits on it. Off by default.
DURATION is a whole number of seconds (`s`, or no unit), minutes (`m`) or
hours (`h`), at least `1m` and at most `168h`. The option needs
`outbound_enabled` and an `outbound_command` helper, and `run` refuses to
start without them.

The draft is watched at each 10-second scan while some `pending` request's
last attempt was refused because the composer held text. Any change to the
draft starts the wait again, as does a scan that finds no such request. When
the draft is due, nothing happens while herdr reports the agent other than
`idle` or `done` or its screen shows or may show a running turn. Otherwise
`run` takes the pane's delivery lock, which every agentctl sender takes while
it types, reads the composer again, and goes on only if the draft is the same:

1. It writes the draft to a new file of mode 0600 in `composer-backups/` in
   the state directory (created with mode 0700), named by the UTC time.
2. It deletes the draft with the editing keys described above, never with a
   key that submits, interrupts or opens a menu, and reads the screen to
   prove the composer empty. If that fails, it stops here: no notice is
   posted, the backup keeps the whole draft, and the wait starts again.
3. It posts one root message to the first configured channel through the
   outbound helper, using no model:

   ```text
   [chat-bridge WARNING] The following text was left in the input box and cleared out automatically after not changing for 30 minutes:
   > the draft, each line quoted
   ```

   The duration in the notice is DURATION. A draft too long for one message is
   cut at a line, and the notice says so. A failure the helper reports as not
   applied and retryable is tried twice more under the same operation ID; any
   other failure is logged and the notice is not posted again.
4. The waiting requests are retried at once.

The service log describes each step by the draft's length; the draft's text
goes only to the backup file and the notice. The composer is read as drawn,
so a draft wider than the pane is saved with a line break where it wraps, and
a paste Claude Code shows as `[Pasted text #N]` is saved as that placeholder.

When the queue types, it goes through the prompts in its inbox in the order
they were queued, checks before each one that it can type it, and stops at the
first it cannot. So a pass can also type prompts that earlier passes left in
the queue, and the pass for a newer request can type an older request's prompt
even when it does not reach its own. A pass records only its own request's
prompt as typed; the scans described next record the others.

Every 10 seconds `run` scans the request records. A scan first asks the queue
what became of the prompt of each `pending` or `submitting` request whose
delivery has started, and records as typed each prompt that the queue reports
typed, as when another request's pass typed it; a prompt that never reached
the queue is reported as nothing, and nothing is recorded. The time it records
is when the scan saw the prompt typed: normally within 10 seconds of the
typing, but later when a pass runs long, when the scans cannot read the queue,
or when the prompt was typed while no service ran. It never changes a
`delivery_uncertain` request. `chat status` asks the queue nothing, so while no
service runs, a prompt typed this way still counts as not typed.

When the scan still finds requests `pending` or `submitting`, `run` looks
herdr's status up, and if herdr reports `idle` or `done`, tries again to type
their prompts. So each scan reads the request records, and while prompts wait,
it also reads their queue entries and makes one herdr status lookup. When the
lookup fails, the service logs one line saying so, goes by the status it
learned before, at a lookup or from herdr's events, and looks it up again at
the next scan, logging one more line when a lookup works again. The retry
handles the requests oldest admission first and stops at the first one whose
prompt still waits, since a drain that stops would stop at the same point for
the rest, unless the rescan for requests that arrive faster than they can be
queued runs at the same time and tries them all itself, in which case the scan
makes no lookup either. The queue still types in the order the prompts were
queued, which need not be admission order: a newer request's prompt that an
earlier pass queued is typed before the prompt that the retry queues for an
older request. The retry tries none while the status the service last learned
is another one: the queue types only in `idle` or `done`, so with the default
`--ready-timeout` of 0 a retry could only fail, and with a longer one it would
hold the service while it waits. When the pane settles idle or done, the
recovery scan that herdr's event starts tries every prompt still waiting. A
retry is a pass that handles queued requests, so the pane is read before it as
described above. The first scan comes when `run` starts, just after the
recovery pass at startup, and tries none itself. That pass handles at most four
requests and leaves the rest to the passes that run as the service goes on, so
the first scan can list prompts that no pass has tried yet.

A request is stalled once 60 seconds have passed since its admission while its
prompt is not recorded as typed, that is, while its phase is not `delivered`.
A `delivery_uncertain` request therefore stays stalled until an operator
settles it, even after the agent has stored or sent a reply to it.

The service logs a line when a scan first finds a request stalled. The line
names the request's key and age and the reason its record keeps, or says
`no reason was recorded`. While the service is still trying to type the prompt,
the line also names the phase; the line for a `delivery_uncertain` request says
instead that its prompt is not typed again. For as long as the request stays
stalled, the service logs it again every 600 seconds, with
`is still not typed after`, and it logs a line again at once whenever a logged
request's delivery becomes uncertain or stops being uncertain. Once a logged
request's prompt is recorded as typed, one more line says so: `typed after`
with the time from admission to the time recorded, even when the request is
retired at once, as can happen when its replies were closed before its prompt
was typed, and even when the scan then fails partway through that retirement.
A logged request that leaves the state directory otherwise, as when
another process retires it, gets one line with `is no longer retained` and the
time since its admission. Ages are rounded to a tenth of a minute, of an hour,
or of a day. A refusal to type over a composer draft quotes the start of the
draft, which is someone's own words, so these lines, `chat status`, and
`delivery-alarm.json` show `(text not shown)` in its place; `chat inspect`
shows the recorded reason as it is.

Each scan also keeps `delivery-alarm.json` in the state directory current. Its
`schema` is `agentctl-chat-delivery-alarm/v1`, `stall_after_seconds` is the age
at which a request is stalled, and `stalled` lists the stalled requests, oldest
admission first, each with its `key`, `phase`, `admitted_at_millis`, and
`reason`, which is null while no attempt has recorded one. The file holds no
ages, so `run` writes it at its first scan and afterwards only when the list
changes: when a request becomes stalled, is typed, or leaves the state
directory, or when a stalled request changes its phase or reason. Only `run`
writes the file, so it does not change while no service runs. A write puts the
new file in place and then syncs the state directory, so a write that fails can
still have replaced the file; after a failed write, each scan writes the file
again until a write works. When a scan cannot read the request records, ask
the queue about a prompt, write the file, or read the reply ID of a request
whose prompt it recorded as typed, the service logs one line saying so, keeps
running, tries again at each scan, and logs one more line when a scan works
again. A scan that cannot write the file still tries the prompts it found
waiting.

`run` also records in `provider-health.json`, in the state directory, whether
the provider subscription and the outbound send path are failing. The
subscription fails each time a provider generation ends, including a stream
the provider closes, and works again once a generation subscribes. The send
path fails each time the outbound helper fails a reply, an acknowledgement or
a ✅ in a way that can be retried or whose outcome is unknown, and works again
once one is sent and its provider ID kept. A provider ID the bridge cannot
keep, such as one with a line break, leaves the operation unresolved and is a
failure of class `provider_receipt_invalid`. A send the helper refuses as not
applied and not retryable answers that one operation and changes neither. A
record that cannot be read does not stop a scan from keeping the others in
`delivery-alarm.json` current, and the service logs the problem. The record
keeps the value last read for it, which `run` saves in
`delivery-alarm-sources.json` in the state directory so that it survives a
restart and a failed write of the alarm; with no value read yet, the alarm
lists the record in `unreadable_records`, by file name, instead of reporting
it clear, and keeps listing it until a read works. For a failing path the record
holds `down_since_millis`, when its first failure in a row happened;
`failures`, how many in a row; `last_failure_at_millis`; `last_error_class`,
the helper's failure code for a send, such as `provider_authorization`, or for
the subscription the provider's message after its last `": "`, such as
`control child closed stdout during AwaitFirstResponse`; and
`last_error`, the newest message. `chat status` reports both paths in
`provider_health`, as `subscription` and `sends`, each null while it works,
with `down_after_failures`. Once a path has failed 2 times in a row, its first
failure and a retry, `delivery-alarm.json` holds it as `subscription_down` or
`send_path_down`, and `chat status` reports the same in `delivery_alarm`. A
routine reconnect, where the provider closes a stream and the next generation
subscribes at once, is one failure and is never reported there.

A deployment can point the plugin and the outbound helper at credential
files, such as a TLS client certificate and key, through the variables in
`subscription_environment` and `outbound_command.environment`. `run` checks
each such file that it can see: every variable whose value is an absolute
path to a regular file. A directory (even one it cannot open), a device, a
socket or a pipe is not a credential file and is skipped, so a setting such
as a home directory never counts. For
each file `run` records whether it exists and can be read and, when it holds
a PEM certificate, that certificate's notAfter. It opens the file once,
without blocking, reads it only to find a `CERTIFICATE` block, and reports
nothing from it but the notAfter; a private key is not parsed. The path
itself is never reported: the bridge keeps the values of the plugin and
helper environment out of its state, status and log. `run` logs each file
once at startup. Each delivery scan saves what it saw in
`provider-credentials.json` whenever that changes, and at least every 300
seconds, with `observed_at_millis`. `chat status` reports that record in
`credentials`, as `evidence`, `stale_after_seconds`, `observed_at_millis` and
`files`, each file with its `variable`, `state` (`present`, `missing`,
`unreadable` for a regular file that cannot be read, or `inaccessible` when
the path cannot be examined at all, as under a directory the service cannot
search), `not_after_millis` (negative before 1970), and `problem`.
`chat status` reads the record rather than its own environment, which may
lack the service's. `evidence` says how far to trust it: `none` before the
service has saved one, `current`, `stale` once the observation is older than
600 seconds (a stopped service, or a previous run's record), `untrusted` when
it is timed later than now (a clock set back), or `unreadable` when the
record cannot be read; `delivery_alarm.credential_evidence` repeats
it, so missing evidence never reads as no problems. A file that is missing, unreadable, expired, or
within 24 hours of its notAfter is listed in `credential_problems` in
`delivery-alarm.json` and in `chat status`'s `delivery_alarm`, even while the
provider still works, so the problem shows a day before the outage. When a
subscription or send failure happens while a file is missing, unreadable or
expired, `last_error_class` names that file (an inaccessible path, not known
to be a file, is reported but never named as the cause), as in
`credential file missing: SOME_TLS_CERT_PATH` or
`credential expired 12 h ago: SOME_TLS_CERT_PATH`, and the provider's own
message stays in `last_error`. The files are read before the state lock is
taken. Renewing a credential is the deployment's job, described in its
plugin's documentation; agentctl only reports it.

`chat status` reports the same list in `delivery_alarm`, as
`stall_after_seconds` and `stalled`, computed from the request records at the
time of the call; each of its entries also carries `age_seconds`, the whole
seconds since admission. Its `deliveries` object counts the retained requests:
`replied`, those with at least one reply sent; `admitted_not_typed`, the others
whose prompts are not recorded as typed; and `typed_not_replied`, the rest. A
reply counts as sent once its own record says so, even if the service stopped
before it advanced the request's count of sent replies. Its `requests` array
lists every retained request, oldest admission first, with `key`, `phase`,
`admitted_at_millis`, `ack_completed_at_millis`, `delivered_at_millis`,
`first_reply_sent_at_millis`, `replies_captured`, `replies_sent`,
`replies_closed`, `age_seconds`, and `reason`. `first_reply_sent_at_millis` is
null while no reply is sent or when the first reply's record cannot be read,
`replies_closed` says whether the request is closed, and `age_seconds` and
`reason` are null once the prompt is recorded as typed. Like the rest of
`chat status`, these need no running service.

When `ack_reaction` is set, the service also adds ✅ to a request's Chat
message once the queue has verified the request's prompt against the agent's
composer: the pane printed it, or it left the composer the queue checked before
typing. After the queue presses
the key that submits a prompt, it looks on the screen for something the pane
printed after the key: more copies above the composer of the whole prompt,
blanks left out, or of a numbered paste placeholder the composer showed for
this prompt, such as `[Pasted text #924 +10 lines]`, than any read before the
key showed; a queued-message marker; or a running-turn marker that no read
before the key showed. Each of these names this prompt: an older prompt built
from the same template is not the whole of this one, and another paste has
another number. A placeholder that names only a length, `[Pasted Content N
chars]`, is not evidence, since another paste can have the same length, and
nor is placeholder text that is part of the prompt itself. A prompt too long to
show whole and shown without a numbered placeholder has printed evidence only
from a marker. A
busy agent shows its running-turn marker before the key and after it, so a
marker that was already showing proves only that the prompt left the composer. When that is all the queue finds, it reads the screen
every 100 milliseconds for up to 2 more seconds, pressing no other key, for
printed evidence. If one of those reads shows the prompt in the composer again,
the empty composer was a passing redraw and nothing is proved: the queue goes
back to pressing the key on its schedule, and reports the prompt as not
submitted if it is still there at the deadline. Otherwise the prompt is recorded
as typed either way. It earns ✅ with printed evidence, or without it once the
whole 2-second wait has passed with its last read showing a recognisable
composer without the prompt: the queue typed it only into a composer it
recognised as empty and ready, so a prompt that left that composer and stayed
out of it was submitted, or queued by the agent. A busy Claude Code that already
holds a queued prompt shows nothing of the next one it queues, so such a prompt
has no printed evidence; it still earns ✅. A wait cut short, because the service
is stopping or a read of the pane failed, or one that ends on a screen with no
recognisable composer, earns none, since nothing then showed that the prompt
did not come back.
Only a submission checked against the Claude Code or Codex composer, as
described here, is verified this way. Any other
submission is recorded as typed once the pane's own confirmation succeeds:
herdr reporting the pane `working`, or the check of its screen after the key
that some panes' adapters make. That confirmation does not check the composer,
so such a prompt earns no ✅. The delivery
alarm above covers a prompt that is not recorded as typed at all.

`chat init` refuses ✅ as the `ack_reaction`, since ✅ marks a verified prompt. A
state made before with ✅ as its acknowledgement still opens; its
acknowledgement already shows ✅ once the request is acknowledged, and no
separate receipt is added.

Each prompt that a drain types and the queue verifies earns its request
✅, subject to the limits below, including the other requests' prompts that the
drain reaches and those it types while it delivers a routing-error prompt. A request admitted while
`ack_reaction` was `null` earns none. The service saves the ✅ as soon as the
queue verifies the prompt, before the queue records the prompt as
processed and before the pass records it as typed, as
`receipt-reactions/KEY.json` in the bridge state directory, where KEY is the
request's key, with the request's channel and message and an operation ID that
every attempt to add the reaction reuses, so that a retry after a failure or a
restart repeats one operation. The file is removed once the helper reports the
reaction added. Saving does not wait for the reaction, and a request can retire
before its ✅ is added, since the file names the message itself. A verified
prompt earns none when the process ends after the queue verifies it and
before the file is written, or when the file cannot be written, which the pass
reports as an error.

At most 2,048 ✅ reactions wait at once. Prompt delivery and chat intake never
wait for room: a ✅ earned while 2,048 wait is lost, and the loss is counted in
`receipt-reactions-lost.json` in the state directory. `chat status` reports the
count as `lost` in `receipt_reactions`, with `oldest_lost_key`, the key of the
first request that lost one, and as `receipt_reactions_lost` in
`delivery_alarm`, with `count` and `oldest_key`. In `chat run`,
`delivery-alarm.json` holds the same `receipt_reactions_lost` once any ✅ is
lost; as for stalled requests, only `run` writes that file. The count is kept
for the life of the state directory. When losses start, the pass that loses
the first ✅ puts one line in `receipt_loss_alerts`, naming the request, and no
other until a ✅ has been saved since; `chat run` logs that line, and `chat
tick` prints it in its report. A pass also lists the keys whose ✅ it lost in
`receipts_lost`.

In `chat run`, the worker that adds reaction ACKs, described under
Configuration, adds the ✅ reactions too, one at a time through its bounded
queue, after every ACK it has waiting, including those that did not fit in its
queue and those whose retry is due. A ✅ that does not fit stays saved, and the
worker lists the saved ones again after draining its queue. A failed or
uncertain ✅ keeps its file and its operation ID and waits at least 60 seconds
before the worker tries it again; the file records when that attempt was made,
so a restart waits out the rest of the 60 seconds too. A ✅ that the outbound
helper refuses as not applied and not retryable, as a helper that accepts only
the configured `ack_reaction` does, is not tried again: its file is removed,
the service logs the refusal once, and it is counted in
`receipt-reactions-refused.json` in the state directory. `chat status` reports
the count as `refused` in `receipt_reactions`, with `oldest_refused_key` and
`last_refusal`, the helper's newest reason, and as `receipt_reactions_refused`
in `delivery_alarm`; in `chat run`, `delivery-alarm.json` holds the same once
any ✅ is refused. When `run` starts, it
hands the worker every ✅ already saved. `chat tick` adds at most four saved ✅
reactions each time it runs, least recently tried first, so one that always
fails does not hold back the rest. The outbound helper receives each as the
same `ensure_reaction` request as an ACK, with `✅` as its emoji, so a helper
must accept ✅ as well as the configured `ack_reaction`. A saved reaction's
path, `receipt-reactions/KEY.json`, that holds something other than a regular
file is not opened and counts as failing; nor is the record of losses opened
when it is not a regular file. `chat status` reports `receipt_reactions`, with `reaction`, which is
`✅`; `waiting`, the number saved and not yet added; `failing`, how many of
those failed at their last attempt or cannot be read; and the `lost`,
`oldest_lost_key`, `refused`, `oldest_refused_key` and `last_refusal` above.

## Service management

`chat run` is a foreground process and exits cleanly after SIGINT or SIGTERM.
A service manager should restart it on failure and use the same state directory.
Size task and memory limits for the selected provider implementation; those
costs are outside the provider-neutral host and can differ substantially between
plugins. Disable swap for a latency-sensitive bridge only after giving the
provider enough physical-memory headroom.

`chat run` logs to standard error, and every line it writes begins with the UTC
time, to the second, at which it was written, so a service manager that appends
standard error to a plain file still records when each event happened. That
includes its final error line and every line of `graceful-stop-main`. Two kinds
of output have no time: an error in the command-line arguments, which is printed
before the command is known, and a panic message. Other commands, `chat tick`
among them, print their lines without a time. Each log line is written in one
piece, and a line that cannot be written, for example because the disk is full,
is dropped; the service keeps running. The provider worker logs one line each
time a provider subscription opens, such as
`2026-09-30T01:00:36Z agentctl: chat provider: subscribed from the saved cursor`,
and one line each time a subscription ends or fails, naming the reason and the
wait before the next attempt, such as
`agentctl: chat provider: stream ended; reconnecting in 1s` after the time. An
attempt runs from the start of connecting until its subscription ends or fails.
The provider reconnect wait starts at 1 second and doubles after each attempt
that lasted less than 60 seconds, up to 60 seconds. An attempt that lasted at
least 60 seconds counts as healthy, so the wait after it starts again at 1
second. A failed Herdr output subscription is logged the same way, with
`retrying in` and its wait. If the provider worker itself ends or panics while
the service is not stopping, `chat run` exits with an error, such as `chat
provider worker stopped unexpectedly`, so the service manager restarts it
instead of the bridge running on without chat events.

Derive the hard stop interval from the selected plugin manifest rather than
copying a universal number. Before Hello completes, the host may need
`hello_seconds + close_seconds + shutdown_grace_seconds + 7` seconds. After
Hello, it may need `close_seconds + shutdown_grace_seconds + 7`. The final seven
seconds reserve the process supervisor's two-second forced-reap bound and five
seconds for host reconciliation and worker joins. Start is interruptible after
Hello, so its independent phase timeout is not additive. An admitted reaction
or reply helper separately owns `outbound_command.timeout_millis +
outbound_command.shutdown_grace_millis + 7000` milliseconds. Set
`TimeoutStopSec` to at least the maximum of the applicable provider window and
that outbound window, plus measured service-manager scheduling margin. The
maximum accepted outbound window is 67 seconds. At startup the host pins the
complete selected provider timeout tuple. A later generation whose manifest
differs is rejected until the service restarts, so the configured outer bound
cannot silently become stale.

With systemd 258 or newer, a synchronous same-binary `ExecStop` can address only
the exact main-process identity. `graceful-stop-main` opens a pidfd for
systemd's `MAINPID`, requires its inode to equal `MAINPIDFDID`, sends SIGTERM
through that pidfd, and waits for exact process exit. It does not return at its
70-second diagnostic threshold, because returning would begin a second stop
phase. Pair it with `TimeoutStopFailureMode=kill`: at the one service-manager
deadline, systemd kills the complete control group rather than starting another
grace interval.

For example, a user service can run the same foreground command (replace every
absolute placeholder and tune the resource ceilings from a measured provider
probe):

```ini
[Unit]
Description=agentctl chat bridge
After=herdr.service

[Service]
Type=exec
Environment=AGENTCTL_HOME=/home/USER/.agentctl
ExecStart=/absolute/path/agentctl --registry /work/project/.agentctl chat run --bridge-state /home/USER/.local/state/agentctl/project-chat
ExecStop=/absolute/path/agentctl chat graceful-stop-main --main-pid ${MAINPID} --main-pidfd-id ${MAINPIDFDID}
Restart=on-failure
RestartSec=2
KillMode=control-group
OOMPolicy=kill
# Replace every value below with a measured deployment-specific value.
TimeoutStopSec=<DERIVED_SECONDS_WITH_MARGIN>
TimeoutStopFailureMode=kill
TasksMax=<MEASURED_TASKS_WITH_HEADROOM>
MemoryHigh=<MEASURED_RECLAIM_THRESHOLD>
MemoryMax=<MEASURED_HARD_LIMIT>
MemorySwapMax=0
CPUQuota=<MEASURED_CPU_LIMIT>

[Install]
WantedBy=default.target
```

After installing the unit as `agentctl-chat.service`, make persistence explicit
and verify both properties instead of merely starting an ephemeral process:

```sh
systemctl --user daemon-reload
systemctl --user enable --now agentctl-chat.service
systemctl --user is-enabled agentctl-chat.service
systemctl --user is-active agentctl-chat.service
```

`enable --now` survives a user-manager restart; operation while the user is
logged out additionally requires lingering to be enabled for that account.
`Type=exec` makes an `active` transition contingent on successful executable
startup. `KillMode=control-group` and the one derived stop deadline keep plugin
and helper descendants inside the unit's cleanup boundary. Use the same pinned
`agentctl` executable in `ExecStart` and `ExecStop`; a mutable symlink can make
the control helper disagree with the running generation.

The resource tokens in the example are intentionally invalid placeholders, not
defaults, minimums, or evidence about an unmeasured plugin. Measure the whole
service cgroup—including every plugin and helper descendant—under bounded
end-to-end load. Choose limits with explicit headroom, verify their parsed
systemd values after reload, and then prove the selected envelope still meets
the deployment's latency target. A `TasksMax` below a plugin's startup fan-out
will prevent that generation from launching; an excessively loose limit does
not provide useful containment.

The host bounds retained requests, replies, retirement records, commit receipts, per-frame data, event drains,
mailboxes, process cleanup admissions, and helper deadlines. Ordinary provider
or terminal events touch direct durable records. Full directory scans happen at
startup and explicit/periodic recovery, not in the subscription inner loop.
