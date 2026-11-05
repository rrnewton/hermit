# agentctl chat — message a native coordinator from Google Chat

`agentctl chat` lets you message an existing Codex or Claude agent **running in a
Herdr terminal** from one Google Chat space. Use it to send work or ask for
progress while away from that terminal, and receive the agent's replies in
the originating Chat thread. The agent keeps its native tools, instructions,
conversation, and terminal interface.

One agent is enough: no worker team is required. The agent can also act as a
coordinator and manage workers with `agentctl`; only that coordinator needs chat
access.

## How the connection works

The bridge accepts messages from configured senders and stores them in a durable
queue. By default it polls Google Chat's public REST API. An optional
`event_command` supplies a persistent event stream for prompt intake without
waiting for the next poll. It waits for the target's ready state, then calls
`herdr agent prompt PANE TEXT`. **Herdr submits the text through the terminal's
paste and Enter sequence.** Input does not arrive through a native harness
channel or protocol connection. Herdr must report the subsequent working state
to confirm submission.

For each response, the agent uses the provider-independent reply tags supplied
in the prompt. The bridge captures each complete block and posts it as a separate
message in the original thread. The same request can receive progress updates
followed by a final answer. The agent needs no file write or reply-command
invocation for this normal path.

`agentctl chat run` is a separate, long-running process. The Herdr server owns the
agent's terminal and harness process. The bridge does not launch or restart the
agent: stopping either process leaves the other running. The bridge owns its
local reply state. History and manual recovery commands need access to that
state and, for history, the configured Chat transport.

## Setup

This guide applies when `agentctl capabilities` lists the Chat extension.
The Python distribution includes it; no separate extra or plugin is required.
Herdr must be installed separately with its `agent prompt`, `agent wait`,
pane/session inspection, and `events.subscribe` APIs. The supported command
interface is Herdr 0.8.

Authenticate your harness and start a dedicated coordinator:

```sh
agentctl start coordinator --harness codex --cwd /work/project \
  --registry /work/project/.agentctl
agentctl status coordinator --registry /work/project/.agentctl
```

Use the returned pane and workspace identity in `chat.json`. Choose a model you
can access with the launch command's `--model` when the harness default is unsuitable.
Use `--harness claude` for a Claude coordinator.
You can instead target a suitable agent already running in Herdr: supply its exact
pane and identity assertions, and omit `agent_name` unless it has a registered
Herdr agent name. No `agentctl` manager process needs to remain running.
Herdr must observe working transitions to confirm prompt delivery. Some Claude
integrations expose session identity but leave the reported state idle during a
turn. In that case the bridge records `delivery_uncertain`; a reply artifact
still proves execution and permits the threaded response without reinjection.

### Launch from the Herdr tab you created

The shortest normal path is to create a shell tab in the desired Herdr workspace,
change to the coordinator's working directory, and run one command there:

```sh
agentctl chat launch --config chat.json --state .agentctl/chat-coordinator \
  --model gpt-6-astra
```

`launch` discovers `HERDR_PANE_ID` and `HERDR_WORKSPACE_ID`, replaces any stale
target in the reusable configuration, initializes new bridge state, starts the
bridge, and runs the native coordinator in that same pane. The bridge lives for
the coordinator's normal lifetime. Its diagnostics go to `bridge.log` under the
state directory. Launch retains only `bridge.log` and `bridge.log.1`, each at
most 1 MiB; an exit diagnostic reads only the final 2,000 bytes of those files.
Exiting the coordinator stops the bridge and returns to the
shell; after killing the launcher externally, check for and stop any surviving
bridge process before reusing its state.

Without an `event_command`, `launch --interval` accepts the same 0.1–86400-second
polling range as `run`; the polling and failure-backoff behavior is described below.

The launched coordinator inherits `HERDR_WORKSPACE_ID`. Consequently, its
ordinary `agentctl start NAME ...` calls create subagent tabs in the same Herdr
workspace unless it explicitly supplies `--workspace-id`.

For `launch`, `chat.json` is reusable and does not need a `target` or
`agent_name`; those fields are replaced from the current pane. A compact public
REST configuration is:

```json
{
  "space": "spaces/YOUR_SPACE_ID",
  "allowed_senders": ["users/YOUR_GOOGLE_USER_ID"],
  "agent_label": "codex-coordinator",
  "ack_reaction": "🤖"
}
```

The configuration must be a private regular file owned by the current account
(normally mode `0600`) and is limited to 512 KiB. The bridge rejects symlinks,
hard links, duplicate keys, nonfinite numbers, excessive JSON nesting, and a
file that changes while its single opened descriptor is read. The same strict
read rules apply to saved bridge artifacts. `allowed_senders` permits at most
256 unique canonical IDs of at most 256 UTF-8 bytes each. Each configured
transport, token, or event command permits at most 128 arguments, 8 KiB per
argument, and 128 KiB in aggregate. Target identity fields are limited to
512 bytes, except `expected_cwd` and socket paths, which permit 4 KiB.

Transport settings such as `token_command`, `transport_socket`, and
`event_command` belong in the same file. `--model` becomes the reply label by
default; use `--agent-label` when the visible label should differ. `--resume`
resumes a native conversation, and repeated `--harness-arg` values pass literal
extra arguments to the harness.

For an inbound-only coordinator, add `"outbound_mode": "disabled"`. The bridge
still polls or subscribes, durably saves authorized messages, supplies thread
context on request, and delivers prompts to the pinned Herdr pane. It never
creates reactions or Chat messages, never opens a reply-capture subscription,
and refuses `agentctl chat reply` artifacts. The prompt states that outbound
Chat is disabled and that no Chat reply will be published. This mode overrides
`ack_reaction` and `reply_mode` defensively, including for pending-looking state
left in the state directory. `"enabled"` is the default and the only other
accepted value. A REST deployment using disabled mode needs message-read scope,
not message-create or reaction-create scope.

Run `launch` only from an idle Herdr shell pane. It refuses a non-Herdr terminal,
a mismatched workspace environment, a pane already hosting an agent, a missing
harness executable, or an already initialized state directory. Use one bridge
per Chat space: stop the old coordinator before launching a replacement against
the same space, and give the replacement a new state directory.

Routing is per space, not per thread. Every authorized message in the space,
in any thread, goes to the bridge's one target agent. To give several managed
agents their own conversations, give each agent its own space, its own
`chat.json`, and its own state directory. Two bridges cannot split one shared
space by thread, because each would ingest every thread. To connect an existing
managed agent rather than launching a new coordinator, use the two-process setup
below and put that agent's pane identity in `target`.

The explicit two-process setup below remains useful when a service manager owns
the bridge independently of the coordinator.

```json
{
  "space": "spaces/YOUR_SPACE_ID",
  "allowed_senders": ["users/YOUR_GOOGLE_USER_ID"],
  "agent_label": "your-coordinator-model",
  "ack_reaction": "🤖",
  "reply_mode": "tagged",
  "agent_name": "coordinator",
  "target": {
    "pane_id": "RETURNED_PANE_ID",
    "expected_agent": "codex",
    "expected_workspace": "YOUR_WORKSPACE_LABEL",
    "expected_cwd": "/work/project"
  }
}
```

`agent_name` binds delivery to the live named Herdr coordinator. Its pane is
checked before polling, at every readiness probe, and again before submission.
Delivery stops if the name no longer owns the expected pane. Give a replacement
coordinator a new name and bridge state instead of reusing that identity.

Add `session_agent` and `session_value` when the harness reports a stable session.
These are identity assertions, so a mismatch stops delivery. Keep the pane
dedicated to the coordinator and leave its input composer empty between turns.

For the built-in public Google Chat REST transport, provide an OAuth access token
in `HERDR_CHAT_TOKEN`. The token needs permission to list messages in the selected
space, create replies, and create reactions. The built-in transport uses user
authentication. A suitable scope combination is `chat.messages.readonly`,
`chat.messages.create`, and `chat.messages.reactions.create` (each under
`https://www.googleapis.com/auth/`). The broader `chat.messages` scope also
covers these operations. Add that user to the space. Token acquisition and refresh
belong to your deployment; tokens are never saved in bridge state. For automatic
refresh, set `token_command` to an argument array such as
`["gcloud", "auth", "print-access-token"]`; it runs before every transport operation and
must print only the current token to stdout. `token_env` selects another
environment variable when needed. Renewing an environment-only token
requires restarting the bridge with the new environment. Alternatively,
configure a command or socket transport below to use an existing authenticated
client. Tokens are limited to 16 KiB; a token helper's stdout is bounded at that
credential-sized limit plus its optional line ending rather than the general
adapter-output limit.

```sh
agentctl chat init --config chat.json --state /work/project/.agentctl/.chat
agentctl chat run --state /work/project/.agentctl/.chat
```

Initialization sets the message history cutoff to the current time. Messages
created before that cutoff are excluded from both events and polls. Use an
explicit RFC3339 `--after` on `init` to replay a chosen interval. Without `event_command`,
`run` waits one hour **after each completed polling cycle** before starting
another by default; `--interval` accepts 0.1–86400 seconds (one day). A cycle includes transport and
delivery work, so this is not a fixed message latency. Choose a longer interval
when your authenticated client has a shared read quota or a costly polling
adapter. For configured intervals at or below 60 seconds, failures double the
delay up to 60 seconds. Longer configured intervals double on failure up to one
day. Backoff never shortens the configured interval, and a successful cycle
restores it.

With `event_command`, incoming events wake the bridge immediately. REST
reconciliation runs on connection, after a reported event gap, and at
`--reconcile-interval` (10–86400 seconds, default 300); `--interval` does not
apply in this mode. Independently, a local durable-state recovery pass runs at
startup and every 300 seconds even when REST reconciliation is configured less
often. ACK requests,
Herdr prompt delivery, replies, and recovery scans run independently:
a busy coordinator does not delay an intake ACK, and a slow ACK does not delay
prompt delivery. Provider and harness latency still determine when those
operations finish. See the event adapter contract below to enable this mode.

Reply capture waits on Herdr subscriptions in both modes. A service manager can
restart the process using the same state directory. `status` displays requests,
delivery phases, ACK retry state, capture errors, and observer state. The first
observed state is durable immediately. Later connection/error flaps are
coalesced, and the latest state is saved after at most 60 seconds by default.
`--observer-write-interval` accepts 60–3600 seconds for both `run` and `launch`.
It is a hard minimum between advisory state writes, not a poll-frequency
heartbeat: unchanged input and output state causes no write. Each completed REST
reconciliation advances the in-memory `reconciled_at` diagnostic. The first
success and then at most one quiet success per monotonic hour run the
staged+intent publisher; after a restart, the durable `updated_at` anchors the
remaining delay, and a future anchor caused by a backward clock step waits a
full monotonic hour. The next necessary state or cursor write also folds in the
latest completion. Reconciliation error flaps are coalesced like other advisory state,
so disk records the latest net state after the write deadline rather than every
transient edge. Input cursor advances remain immediately
durable regardless of this interval. Use the service supervisor for process
liveness; `updated_at` records the latest persisted state transition. Older
releases could save longer observer errors. A compatible file within the 8 KiB
cap is shortened once to the current 2,000-character bound; a file beyond the
encoded cap is refused instead of being read and rewritten. Only one
`run` or `tick` process may own a state directory: `.run.lock` enforces this.
Stop `run` before using `tick` for one polling, capture, and delivery cycle.
SIGTERM performs owned stream/process cleanup and exits successfully for service
managers; interactive SIGINT/Ctrl-C remains exit status 130.

Messages are accepted only from the configured sender resource IDs. A distinct
bot identity for replies makes authorship clearer. Durable reply IDs suppress the
bridge's own messages even when an adapter posts as an allowlisted user. Streaming
intake defers a possible reply echo until its pending send is reconciled. A command
transport must preserve Google sender IDs rather than replace them with display
names. The bridge does not authorize messages from everyone in a space.

## Reply capture

`reply_mode` defaults to `"tagged"` for newly ingested requests. New requests use
reply protocol v3. Each prompt supplies an unpredictable 22-character request
nonce and the exact first reply ID, whose numeric ordinal starts at 1. The agent
may reply once or multiple times, including progress updates when requested.
Each block becomes a separate chat message. Put each tag on its own line,
without an enclosing code fence, and increment the ordinal for every later reply:

```text
<CHAT_REPLY_NONCE_1>
The implementation is ready. I am running the checks.
</CHAT_REPLY_NONCE_1>

<CHAT_REPLY_NONCE_2>
The checks passed. The change is ready.
</CHAT_REPLY_NONCE_2>
```

`NONCE` stands for the unique value in that request's actual tags; do not reuse
this example literally. New requests remain available for further replies after
earlier replies are sent; a closing tag ends one message, not the request. A
later user request does not invalidate the earlier request nonce. Ordinals must
be consecutive and may not be skipped, duplicated, or reused with different
text. Capture closes automatically after bounded ordinal 999999. `GCHAT_REPLY`
spelling remains accepted, while new prompts use `CHAT_REPLY`.

The daemon keeps one Herdr `events.subscribe` connection with one line-local
predicate for each active request's exact next closing marker (at most 128 active
requests). After a real retained-output snapshot is captured durably, it
immediately rebuilds the subscription for that request's next ordinal. A single
snapshot may capture several consecutive ordinals. There is no periodic output
reconnect. Idle/done status events perform a fresh retained-output read, which
also recovers final output after scrollback retention changes. Herdr checks text
subscriptions internally every 100 milliseconds; this is separate from the
chat provider's polling interval. Retained terminal history is bounded, so
capture is not a lossless stream of agent messages.

Exact-next predicates guarantee timely valid progress without a 1 Hz scan, but
an unavailable or malformed marker does not itself wake an exact active-request
predicate. It is diagnosed when it appears in a snapshot triggered by a valid
marker or an idle/done read. With no active request, a generic diagnostic
predicate remains installed. A malformed block whose exact closing marker is
visible still records `capture_error` immediately.

Protocol v3 is a deliberate state migration. `run` refuses before provider or
Herdr access when a still-addressable unsequenced v2 (or older pre-versioned)
request exists. Stop the runner, recover retained unsequenced work with one-shot
`agentctl chat tick` or a v2-capable bridge, then close each v2
subscription without deleting history:

```sh
agentctl chat close --state /work/project/.agentctl/.chat \
  --request REQUEST_KEY_OR_UNIQUE_HEX_PREFIX
```

The same `close` command closes a v3 request explicitly. `status` keeps the
record, reply history, `reply_closed_at`, and closure reason; its
`reply_subscriptions` object reports the current active count and limit. Closing
requests frees the 128-request subscription capacity. If v2 recovery is not needed,
initialize a fresh state directory and retain the old directory for inspection;
do not erase it to bypass migration.

### Resource and complexity model

Let **R** be retained request records, **Q** retained harness-queue artifacts,
**F** retained routing-feedback records, **D** deferred possible-echo records,
**S** unadopted local file submissions, **P** mutable pending-reply journals,
**M** lifetime reply-history artifacts, **B** their retained body bytes, and
**L** the retained pane text (hard-capped at 2 MiB). In push mode, unchanged
provider heartbeats are discarded before the owner mailbox. Between real events
and recovery deadlines there is no 1 Hz filesystem scan, subscription rebuild,
or durable observer heartbeat. The owner may wake after at most 30 seconds to
observe an externally set stop flag; that deadline check performs O(R) in-memory
work and no filesystem scan. Cursor-changing events write their checkpoint
immediately. Pending harness delivery keeps an idle/done status subscription
even with outbound Chat disabled; its output-pattern list is empty, so this
does not capture terminal text or publish replies. A settled event wakes a
readiness-checked drain immediately. A missed event is covered by a slow
300-second delivery fallback, not a 30-second busy retry. Repeated unchanged
busy errors preserve their existing artifact and timestamp without another
atomic write or fsync.

The retained intake population is capped at 2,048 request records, 64 MiB
of complete normalized source-message JSON, and 128 MiB of encoded request
files, with each normalized message capped at 64 KiB (including resource IDs,
timestamps, optional fields, and text). Request records use an exact
version-aware field schema, so unknown padding cannot bypass these budgets.
Startup and recovery stream this population and refuse before constructing an
over-limit in-memory list. They also cap F at 1,024 records/16 MiB, D at 1,024
records/64 MiB, and Q at the derived R+F count with at most 512 KiB per artifact
and a 512 MiB reservation budget. Reservations include escaped JSON prompt
bytes, bounded delivery metadata, error sidecars, and temporary atomic replacement
files. Delivery errors are capped at 2,000 UTF-8 bytes before persistence. Intake
reserves a complete request batch before writing any new request or advancing its
cursor; retries reuse the same per-ID reservation. Recovery counts retained metadata
and sidecars before retaining the queue, and refuses any prompt whose encoded
artifact plus update allowance cannot fit. Thus inbound-only mode has the same bounds.
Recovery also counts crash-left `.message.*` files in every queue directory;
their population is limited to the request/feedback ceilings plus 16, and their
bytes consume the same budget. A fixed allowance covers target binding,
its atomic replacement, and lock files. Refusal preserves these artifacts for
operator inspection; it does not remove potentially active temporary files.
All new Chat JSON writes, including submissions and queue metadata, share one
private `.atomic/staged.json` slot under `.atomic.lock`. The lock spans bounded
serialization, write/fsync, destination replacement or create-only link, and
directory fsync. This adds at most one 512 KiB payload plus one 8 KiB recovery
intent per state; internal publication links share the same payload inode,
rather than one crash-left file per write or destination. Generic queue callers
retain their existing behavior unless they explicitly select this policy. A
selected policy also bounds queue reads, writes, and diagnostics when no separate
artifact limit is supplied; an explicit limit cannot enlarge the policy bound.
`send` applies the same policy to binding, enqueue, and drain. The staging
directory and lock pathname cannot be write destinations; only the fixed audit
marker is allowed inside the staging directory.
Before publication, the writer durably records the confined destination and
payload identity. It retains the staging link until the destination directory
has been fsynced. Recovery retries that fsync before adoption, delivery, or a
checkpoint may proceed. Before removing any internal payload link, cleanup
appends a typed, content-free PREPARE phase to the intent and fsyncs both the intent and
staging directory. The phase shares the existing 8 KiB metadata bound; a failed
preparation preserves the original intent bytes and all payload links. Cleanup
then removes internal links, fsyncs, and revalidates every held inode and the
final name while the phase is still durable. Only after those post-unlink checks
and cleanup fsync succeed does it append and fsync COMMIT. Unlinking the intent is the
commit point and final namespace operation; only a directory fsync follows it.
The next writer may reclaim only those fixed, owned, mode-0600 regular entries;
an unsafe or oversized slot is refused and preserved. If a create-only write
crashed after linking its final name, reclaiming the slot preserves the final
artifact. Corrupt post-publication intent is refused without discarding evidence.
A malformed partial intent can be reclaimed only while its owned payload has
exactly one link and no publication link exists. It uses the same durable cleanup
phase before removing the payload. PREPARE recovery resumes only when a trusted
payload, publication link, or matching final remains. Between last-anchor unlink
and durable COMMIT, a crash can leave no trusted anchor; that irreducibly ambiguous
state requires offline inspection. A complete COMMIT certifies the post-unlink
checks and permits remaining intent cleanup. A partial COMMIT is only PREPARE,
not a certificate. Detected interference remains refused across restart. Errors
report bounded diagnostics and digests, not payload text. These are
interference checks, not exclusion of arbitrary same-UID writers: a transient
alias hidden by our own unlink's ctime update, or changes after the final check,
cannot be atomically ruled out on Linux. No later semantic validation relies on
reconstructing recovery evidence after the commit point.

At first owner startup, a bounded migration audit holds the runner, submission,
queue-binding, queue-delivery, and atomic locks in that order. Normal queue
operations release binding before taking delivery, so the audit adds no reverse
lock dependency. It reports scattered `.message.*`
files with paths and byte counts for offline cleanup; it never glob-deletes
them. The audit refuses unsafe entries, more than 128 scattered temporaries,
or more than 64 MiB. A durable `.atomic/audited-v1.json` marker avoids later
history-directory walks. Submission commands audit their own directory under
the submission lock until the owner has completed that migration. Ordinary
event-driven writes check only the fixed slot and do not scan staging directories.
`request-limit.json` and `population-limit.json` retain coalesced refusal
diagnostics. At exhaustion, already accepted work remains durable, while the
new message is refused without advancing its event or REST checkpoint. Drain
the old state and rotate to a fresh state directory; raising a service memory
limit does not repair an over-limit state.

Every durable JSON body also has an individual encoded-file cap: 512 KiB for
`bridge.json`, request, feedback, deferred, and current delivery-queue records;
32 KiB for `input.json`; 8 KiB for `output.json` and reply receipts; and 256 KiB
for pending/history reply items and local submissions. Coalesced refusal
diagnostics are limited to 64 KiB. Each older embedded reply outbox that
otherwise validates may be migrated up to 256 MiB, and all such encoded
monoliths together may consume at most the 1 GiB state-wide reply budget. The
bridge removes each monolith durably after its sharded summary verifies; a
failed cleanup is retried from that current summary without remigrating. Any
larger older-format outbox or saved artifact is refused. Writes are finite JSON
and are size-checked before a
temporary is created. Reads use one no-follow, nonblocking descriptor, require a
private current-user regular file with one link, read at most cap plus one byte,
and reject duplicate keys, nonfinite numbers, excessive nesting, and an unstable
opened file. REST and command-adapter JSON responses are limited to 8 MiB, and
poll, stream, and context cursors to 8 KiB.

Regardless of `--reconcile-interval`, an unconditional local durable-state
recovery pass every 300 seconds rereads and sorts request records, costing
O(R log R), and inspects O(Q + F + D + P) queue/feedback/deferred/pending
artifacts plus O(P x 30 KiB) pending body bytes needed for recovery. Runtime
recovery decodes each R/Q/F/D artifact once; the queue's delivery lock keeps
phase names stable while the same auxiliary pass builds prompt, feedback, and
deferred indexes. Queue bodies are discarded after their small prompt index
entries are derived.
Cold validation and status do not build these runtime caches: they retain only
the current auxiliary body and scalar identity/reservation indexes. Usage and
reservations become admission authority only after a complete validated scan;
any read failure or refusal invalidates the cached authority. Reply-text
index rebuilds touch each pending key once, not the accumulated index once per
request. Both sharded and embedded reply summaries must pass per-request
and state-wide count/byte caps before any pending bodies are materialized. It does
not enumerate immutable sent history, so its cost is independent of M and B. This is
distinct from configurable provider reconciliation. Each completed REST
reconciliation also refreshes these local indexes at the same asymptotic cost,
so choosing an interval below 300 seconds deliberately increases scan cadence.
Fully idle push operation is therefore not scan-free.

Continuous owners keep a small `(literal line, digest)` index for Q prompts,
not a second in-memory copy of every prompt body. A valid output event builds
the visible-line set in O(L), checks Q small anchors, and exact-reads only the
V queue artifacts whose anchors are actually visible; it never scans a queue,
feedback, deferred, or reply-history directory. Exact full-prompt matching and
removal cost O(V x L), so ordinary output CPU is O(Q + V x L + R + L), with
V <= Q. One-shot `tick`/capture, which has no long-lived owner cache, may read
all Q queue artifacts once. Feedback and deferred possible echoes are indexed
at startup/recovery and updated by exact event IDs; their lifetime directories
are not rescanned after each output event. Output capture writes one pending
artifact and a constant-size request summary per new block. Send
completion updates that pending journal, creates one immutable history artifact
and exact provider-ID receipt, updates the request summary, then removes the
pending journal. These durability operations are O(1) artifacts and O(body)
bytes per reply rather than rewriting prior bodies.
The streaming owner reuses its request cache and rereads only records changed by
capture. Marker advances pass an immutable owner-authorized snapshot to the
subscription worker, which rechecks target identity without request-directory
scans or reads. Public one-shot opens still validate durable state. Polling
shares one validated request snapshot between marker selection, capture, and
subscription open/reopen; unchanged waits do not reload it. Each snapshot
streams and checks count/byte caps before retaining each record. A released
batch of D deferred messages uses one request-population preflight, O(R + D),
and leaves all deferred sources intact if admission fails. Replaying an
unchanged retained snapshot does not rewrite request metadata. A
malformed multi-request snapshot can invoke conservative per-request parser
isolation, with an adversarial O(R x L) fallback; an unavailable-ID diagnostic
also sorts at most R destinations once, at O(R log R). Request/deadline staging
retains an O(R) CPU pass per real event or deadline. Command transports are
bounded to 60 seconds, 8 MiB stdout, and 64 KiB stderr; crossing a bound terminates the
helper process group, including descendants left behind by a successful leader.
On Linux, bounded command helpers require `pidfd_open` and process-descriptor
signals. Unavailable descriptors or descriptor exhaustion refuse execution
before the adapter receives permission to start. A trusted supervisor publishes
a separate process-group anchor; the caller validates both identities before
acknowledging execution. The supervisor kills and reaps the original group and
returns the adapter's exact exit or signal status over a private bounded channel.
Before spawning it, the caller reads the exact Linux `sigaction` and refuses
unless `SIGCHLD` is `SIG_DFL` with `SA_NOCLDWAIT` clear; otherwise a direct child
could be auto-reaped and its numeric PID reused during a descriptor-failure race.
Missing or malformed status is a failure. Cancellation first asks the supervisor
to clean up. Emergency cleanup freezes and checks the exact identities, takes a
stable stopped group census,
and requires all recorded processes to disappear. Caller death also wakes a
stopped supervisor so it can clean up. Descendants deliberately escaping into
another session or process group are outside this containment contract.
`chat launch` likewise blocks on pidfds for the bridge and coordinator instead
of polling either process. A bridge-only exit gets one event-driven 200 ms
coalescing wait for coordinator completion, matching the former observation
window without waking while both processes remain alive. Each launch child first
runs an inert parent-death gate. The bridge or coordinator command cannot execute
until the launcher validates the gate's identity, owns its pidfd, and receives the
gate's bounded readiness byte proving its double-checked parent-death lifeline is
armed. SIGTERM during either `Popen` window is recorded through a self-pipe and
acted on only after the new child is tracked. Pidfds remain open through exact
status collection and reaping. If both post-spawn pidfd attempts fail, cleanup
first revalidates the still-unreaped direct child's PID, start time, and PPID before
waking, killing, reaping, and proving it disappeared. Bridge process-group signals
additionally require its still-live or unreaped leader identity to match the
recorded PID, start time, session, and PGID.
Launch applies the same pre-spawn `SIGCHLD` requirement, because automatic
reaping could destroy that zombie pin and its authoritative wait status.

Persistent event commands run behind a trusted supervisor and live process-group
anchor. The provider does not exist until the bridge has acquired and validated
pidfds for both helpers and acknowledged their identities; only then can it
receive the subscription request. The supervisor reports the provider's exact
final status on a separate bounded channel. Inherited automatic child reaping is
rejected before the supervisor or provider exists. If descriptor acquisition or
registration is unavailable, the attempt fails closed and the input runner
applies its bounded reconnect backoff; there is no periodic `waitid` fallback.
One-shot and persistent supervisors capture their direct-child identity before
pidfd acquisition. If both post-spawn attempts fail, cleanup first closes ACK
and lifeline authority, then revalidates PID, start time, PPID, process group,
and session before each numeric wake or kill. It reaps and proves the supervisor
disappeared before preserving the original descriptor error, while the closed
ACK and lifeline plus supervisor death remove any pre-ACK anchor.

Reply bodies are limited to 30,000 UTF-8 bytes. Operational caps are 4,096
replies or 64 MiB per request, 65,536 replies or 1 GiB per state, and 1,024
pending replies or 32 MiB per state. Reaching a cap closes further tagged
capture with `reply_close_reason: "storage_limit"` while retaining history and
continuing delivery of already-pending replies. The protocol ordinal ceiling
999999 is not the operational storage budget. The create-only local submission
staging area is independently bounded to 1,024 files and 32 MiB while a runner
is stopped; `chat reply` checks that bounded S population under its own local
lock at O(S) files and O(S x 30 KiB) decoded body bytes. This is an explicit
local CLI operation, not a service-loop scan. Owner adoption applies the
reply/pending caps above. A submission that no
longer fits is recorded by digest/byte count and rejection reason in its request,
then unlinked last so a crash cannot retry or leak the unaccounted body. Default
`status` is O(R) and reports request, auxiliary, and reply counts, bytes, caps,
phase, last reply ID, refusal diagnostics, and closure fields;
it never reads reply bodies. An explicit deep audit reads them at O(M + B):

```sh
agentctl chat history --state /work/project/.agentctl/.chat \
  --request REQUEST_KEY_OR_UNIQUE_HEX_PREFIX
```

The bridge saves each complete tagged block durably before sending it with its
own stable reply ID. It prefixes each posted message with `agent_label`. Each
reply may contain up to 30,000 UTF-8 bytes. Text outside the tags is not sent.
Two complete identical bodies with consecutive ordinals are two messages;
rereading the same ordinal and body does not send it again. A reused ordinal with
different text is refused rather than guessed.

If the agent uses an unavailable reply ID, the bridge queues a protocol error
directly to that agent. The error identifies the unavailable ID and lists the
available request IDs and request-key prefixes so the agent can correct its
routing. The error is not posted to chat. Repeated observations of the same bad
ID and available destinations produce only one error, including after a bridge
restart. Prompt echoes and quoted code examples do not trigger these errors.

Set `reply_mode` to `"file"` to retain explicit file submission. Requests saved
before tagged capture was enabled retain their original file-reply instructions.
`agentctl chat reply` remains available for deliberate recovery in either mode:

```sh
agentctl chat reply --state /work/project/.agentctl/.chat \
  --request REQUEST_KEY --file answer.txt
```

The reply file must be a current-user, single-link regular file containing at
most 30,000 valid UTF-8 bytes. Ordinary user-owned input permissions such as
`0644` are accepted; this explicit operator input is not durable bridge
authority. The command opens it once without following symlinks and refuses
group/world-writable files, hard links, FIFOs, devices, files that grow while
being read, and invalid UTF-8.

You may run `reply` while the daemon is running. After the artifact is durable,
the streaming runner receives a nonblocking private local notification and wakes
immediately. A five-minute recovery scan covers a missed notification or a
restart. The polling runner picks replies up on its next provider cycle. This
pickup interval does not include the provider's send time.
File submission remains a recovery path for one immutable first reply: retrying
the same text is idempotent, and replacing it with different text is refused.
Use tagged output for subsequent replies.

## Thread context

An actual thread reply includes a prompt hint for reading the nearest ten prior
messages. A top-level message does not. The hint uses:

```sh
agentctl chat context --state /work/project/.agentctl/.chat \
  --request REQUEST_KEY_OR_UNIQUE_HEX_PREFIX --limit 10
```

This read-only command needs no live Herdr pane. It uses the saved request's
exact thread and creation time, excludes that request and later messages, and
displays one page in chronological order. It returns source/thread metadata and
an opaque `cursor` when older context is available. Read further back by
repeating the command with the same limit and `--cursor TOKEN`. The default
limit is 10; permitted limits are 1–200.

Context includes all participants in that thread as reference material. It
does not authorize their messages as new tasks. History is fetched only when
the command is run; receiving a message does not automatically load its thread.

## Reaction acknowledgements

The bridge durably saves an authorized input before reacting to its source
message. The default reaction is 🤖. Set `ack_reaction` to a Unicode emoji such
as `"😎"`, or to null or an empty string to disable ACKs. The reaction acknowledges
intake, not completion or guaranteed harness acceptance. Busy coordinators still
receive ACKs while their prompts remain queued. In streaming mode, ACKs and
prompt submissions run independently; neither waits for the other to finish.

Each request saves `ack.state` (`pending`, `acked`, or `disabled`), the configured
emoji, a stable request UUID, attempts, last attempt time, next retry time,
error, and confirmed reaction resource. Reaction failures do not abort prompt
or reply processing. They remain visible in `status` and retry after
3, 6, 12, 24, 48, then 60 seconds while the bridge is running. Restarting preserves
that state and retry identity. Unfinished requests from state created without
ACK support acquire ACK state; completed requests are not decorated retrospectively.

Google's public [reactions.create API](https://developers.google.com/workspace/chat/api/reference/rest/v1/spaces.messages.reactions/create)
has no `requestId` argument. Optionally configure
`reaction_user` as the canonical `users/ID` of the OAuth user. The adapter then
lists that user's matching emoji before creating a reaction, allowing a lost
create response to be reconciled on retry. This needs a reaction-read scope;
`chat.messages.readonly` already permits that read. The OAuth actor can differ
from the allowed sender. `users/me` is not a supported configuration alias.
Without `reaction_user`, creation is attempted directly and unconfirmed failures
stay pending; the bridge does not claim exactly-once reaction delivery or count
somebody else's emoji as its own ACK. Keep the OAuth actor stable across retries.

## Durable state and recovery

State directories are private to the current account. `requests/` holds source
messages and constant-size reply summaries, `queue/` is the existing durable
Herdr inbox, and `submissions/` holds at most one create-only local recovery
submission per request until the bridge owner adopts it; its state-wide backlog
is capped at 1,024 files/32 MiB. Sharded
`replies/items/PREFIX/PREFIX/REQUEST/pending/` journals only unsent work;
`replies/items/PREFIX/PREFIX/REQUEST/history/` holds immutable sent artifacts. Each
sequence directory has at most 1,000 leaves. `reply-receipts/`
provides exact provider-ID lookup without loading history. Older embedded
`replies/REQUEST.json` outboxes are expanded once under the owner lock; the
request storage-version marker is committed last and the older source remains
read-only. A one-time expansion reads that older monolith at O(M + B), but a
normal restart and fixed five-minute recovery inspect only request summaries
and pending journals. Immutable history bodies are read only by explicit
`history`/deep-audit work.
`feedback/` records bounded, deduplicated protocol errors queued to the
coordinator, and `deferred/` holds bounded possible provider echoes until their
send identities reconcile. Immutable message resource names deduplicate both
events and polls; replaying an event does not create a second prompt.
`input.json` records the stream's durable cursor and connection/recovery state.
The cursor advances only after its message has been saved or excluded by the
configured authority. A reconnect passes that saved cursor to the event adapter.
Each delivery pass queues the requests it delivers in source creation order, and
the drain types queued prompts in the order they were queued: a request still
queued from an earlier pass is typed before one a later pass queues, even when
the later one's message was created first. REST pagination is checkpointed after source
messages are persisted, with a 60-second overlap for equal timestamps and brief
indexing delays. Long upstream indexing delays may require an explicit replay with
a new bridge state directory after checking what was already delivered.

Busy coordinators keep messages queued while intake continues. A crash or lost
working-state acknowledgement after terminal injection marks `delivery_uncertain`;
the bridge does not automatically send that prompt again. A keyed reply
from the coordinator proves execution and allows reply delivery while preserving
the failed queue artifact. Otherwise inspect the pane and
queue artifact before deciding whether a new request is needed. Provider failures
and unanswered prompts remain visible in `status`; delivery is not proof of task
completion. A blocked harness may need interaction in its pane.

After a restart or subscription reconnect, the bridge can recover a complete
tagged block only while it remains in Herdr's retained terminal history. This
history is bounded and is not a lossless output log. A visible closing tag with a
clipped or malformed block leaves a `capture_error` instead of posting a guessed
answer. If all tags have been evicted, the request remains awaiting a reply. Inspect
the pane and `agentctl chat status`. An idle/done event triggers one fresh read
to recover an early closing tag inside a quoted example; readiness itself is
never treated as an answer. Output observation stays active after capture errors
without periodic reconnects; later output is recovered on another match or the
next idle/done transition. After stopping `run`, an explicit `agentctl chat tick`
also retries capture once after inspection. Captures request
up to 4,000 retained logical lines
and are bounded to 2 MiB. If the answer cannot be recovered from retained
history, submit the verified answer with `agentctl chat reply`.

Each captured reply uses its own stable UUID request ID. If the upstream
acknowledgement is lost, retries reuse that ID before later replies for the same
request are sent. An identical explicit `reply` retry succeeds even after its
answer was posted. A command adapter **must** honor idempotent sends. Google
Chat's own request-ID retention is finite; inspect very old uncertain replies
before restarting after a long outage. Poll cursors, allowlisted senders, target
assertions, and reply identity are bound to the initialized state; use a new state
directory when changing this authority. Run one bridge per coordinator and space.

## Streaming event adapter

Set optional `event_command` in `chat.json` before initialization to receive
messages from an operator-supplied event adapter:

```json
{"event_command": ["/absolute/path/to/chat-events-adapter"]}
```

This is a separate input connection. Keep either the built-in REST transport,
`transport_command`, or `transport_socket` for reactions, replies, history,
and recovery scans.
The package does not supply a Google Workspace Events subscription or manage its
credentials. One public implementation can use
[Google Workspace Events for Chat](https://developers.google.com/workspace/events/guides/events-chat)
with Google Cloud Pub/Sub, then normalize those notifications to the protocol
below. The adapter owns subscription creation, renewal, upstream acknowledgements,
and any provider-specific reconnect rules.

The bridge launches the literal argument array without a shell. It writes one
JSON object followed by a newline to stdin, then closes stdin:

```json
{"action":"subscribe","space":"spaces/SPACE","cursor":null}
```

`cursor` is null on the first connection, otherwise the last durably processed
cursor string. The adapter must resume from that position or explicitly report
a gap. Replaying the boundary event inclusively is safe. On a first connection,
the adapter chooses its initial stream position. The bridge excludes messages
created before the initialization cutoff, including an explicit `init --after`.
The REST recovery scan fills available history from that cutoff even when the
stream begins at its current position.

Write one UTF-8 JSON object per line to stdout and flush each event promptly:

```json
{"type":"message","message":{"id":"spaces/SPACE/messages/MESSAGE","text":"Please inspect the build","sender":"users/OWNER","thread":"spaces/SPACE/threads/THREAD","created_at":"2026-01-01T00:01:00Z","thread_reply":true},"cursor":"POSITION"}
{"type":"heartbeat"}
{"type":"checkpoint","cursor":"NEXT_POSITION"}
{"type":"gap","reason":"The upstream replay window expired"}
```

`message` has the same normalized fields and identity requirements as a poll
result below. Preserve the authenticated sender ID and exact configured space;
the bridge applies the same sender allowlist to both paths. `thread_reply` must
come from provider metadata. Event objects are exact: `message` requires only
`type`, `message`, and an optional `cursor`; `heartbeat` permits only `type` and
an optional `cursor`; `checkpoint` requires exactly `type` and `cursor`; `gap`
permits `type`, optional `cursor`, and an optional nonempty UTF-8 `reason` of at
most 2,000 bytes. Unknown or cross-type fields are refused before any durable
mutation. An optional cursor must contain 1–8,192 UTF-8 bytes. A `checkpoint`
advances past upstream events that do not produce a message. A `heartbeat`
reports a live connection; it does not prove that no messages were missed. A
`gap` requests a background recovery scan. Preserve event order and emit
preceding inputs before advancing a checkpoint.

Every normalized message has exactly `id`, `text`, `sender`, `thread`, and
`created_at`, with optional boolean `thread_reply`; extra keys are rejected
before echo detection, sender filtering, or cursor advancement. Text is limited
to 32,000 UTF-8 bytes, sender IDs to 256 bytes, message/thread resources to
2 KiB each, and timestamps to 128 bytes. Saved request and deferred messages
must obey the same exact schema; adapter-specific keys make the state invalid
and require operator correction or rotation.

Each line is limited to 1 MiB. Invalid UTF-8, duplicate JSON keys, nonfinite
numbers, or incomplete frames are errors. Stdout must contain only protocol
records; stderr is drained and discarded. The adapter must keep running until
stopped. Handle SIGTERM to unsubscribe and stop owned children: the runner allows
up to two seconds for cleanup before killing the remaining process group.
EOF or a framing failure closes its process group and triggers a new
subscription using the saved cursor, with retry delays increasing up to
60 seconds. Ordinary idle waits do not reconnect. Provider retention and REST
history availability still bound recovery; a durable cursor is not an unlimited
upstream event log.
The event-command runner requires Linux `pidfd_open` and `pidfd_send_signal`.
It verifies descriptor support before creating its trusted helpers, validates
the supervisor and anchor through `/proc` plus mandatory pidfds, and refuses to
start the provider until that containment handshake is acknowledged. Graceful
shutdown signals the provider through its exact handle; the live anchor pins
the original process group through bounded descendant teardown. A missing or
exhausted descriptor, selector-registration failure, or malformed handshake
refuses that attempt. The normal reconnect backoff retries without entering a
process-status polling loop.

## Existing-client transports

This adapter replaces Google Chat access, including authentication and polling.
It does not replace agent delivery: the coordinator must still run in Herdr.
Headless workers and interactive tmux sessions are not supported chat targets.

Set `transport_command` to an argument array in `chat.json`, for example:

```json
{"transport_command": ["/absolute/path/to/chat-adapter"]}
```

The bridge launches the command with literal arguments, sends one JSON object on
stdin, and expects one JSON object on stdout. It never invokes a shell. Each call
must finish within 60 seconds. Stdout is limited to 8 MiB and stderr to 64 KiB;
crossing either bound terminates the adapter process group. Diagnostics belong on
stderr. The adapter owns its credentials and stays separate from the reusable
library.

For an already-running local adapter, set `transport_socket` instead of
`transport_command`:

```json
{"transport_socket": "/absolute/private/directory/chat-adapter.sock"}
```

These fields are mutually exclusive. The socket path must be absolute, and both
the Unix socket and its parent directory must belong to the current account.
The directory must have no group or other access (mode `0700`); where supported,
the connection also checks the peer's account. Start and supervise the adapter
separately; the bridge does not launch its server.

Each operation opens a connection, sends one UTF-8 JSON request followed by a
newline, and reads one JSON response followed by a newline. Successful payloads
are identical to the command transport below; a response containing `error` is
a failed, unconfirmed operation. Socket operations have a 60-second I/O timeout,
with requests bounded to 1 MiB and responses to 8 MiB. Serve connections
concurrently to let ACKs, replies, and scans proceed independently.
Herdr prompt delivery uses its separate connection. This avoids launching a
client process for each request; the adapter can retain its own authenticated
connections.

Poll request and response:

```json
{"action":"poll","space":"spaces/SPACE","after":"2026-01-01T00:00:00Z","cursor":null}
```

```json
{"messages":[{"id":"spaces/SPACE/messages/MESSAGE","text":"Please inspect the build","sender":"users/OWNER","thread":"spaces/SPACE/threads/THREAD","created_at":"2026-01-01T00:01:00Z","thread_reply":true}],"cursor":null}
```

Return messages in ascending creation order. A nonempty cursor continues the same
query; `null` means the page is complete. Sender and thread identity must come from
the authenticated upstream response. Empty/non-text messages may be omitted.
Optional `thread_reply` must be a boolean from the provider's reply metadata,
such as Google Chat's `threadReply`. Only `true` enables the thread-context
prompt hint. A thread resource alone does not establish that a message is a
reply; do not derive this flag or the thread ID from a message ID.

Context request and response:

```json
{"action":"context","space":"spaces/SPACE","thread":"spaces/SPACE/threads/THREAD","before":"2026-01-01T00:01:00.123456789Z","limit":10,"cursor":null}
```

```json
{"messages":[{"id":"spaces/SPACE/messages/EARLIER","text":"The relevant background","sender":"users/PARTICIPANT","thread":"spaces/SPACE/threads/THREAD","created_at":"2026-01-01T00:00:00Z","thread_reply":false}],"cursor":null}
```

Return at most `limit` messages from that exact thread strictly before the
cutoff, in descending creation order. Preserve fractional timestamp precision.
The CLI reverses each page for chronological display. A cursor continues the
same thread, cutoff, limit, and descending order; `null` means no older page.
For the public API, use `createTime < "TIMESTAMP" AND thread.name = THREAD`
with `orderBy=createTime DESC`. Report unsupported history or provider failures
as errors, rather than an empty successful page. Existing poll/send/react-only
adapters continue to deliver messages and replies, but need this action for
`chat context`.

Reaction request and response:

```json
{"action":"react","space":"spaces/SPACE","message":"spaces/SPACE/messages/MESSAGE","emoji":"🤖","request_id":"f17fb68a-5597-49a9-a1ab-d14b26331b0e"}
```

```json
{"id":"spaces/SPACE/messages/MESSAGE/reactions/REACTION"}
```

The adapter must ensure its reaction is present and return that reaction's full
resource name. Repeating the request must not toggle the reaction off. The stable
UUID is separate from every reply UUID and lets an adapter reconcile its own
lost responses. Report failures, including missing reaction permissions, instead
of acknowledging an operation that was not confirmed. Set `ack_reaction` to null
for a transport that does not implement reactions.

Send request and response:

```json
{"action":"send","space":"spaces/SPACE","thread":"spaces/SPACE/threads/THREAD","text":"[model] Finished","request_id":"f17fb68a-5597-49a9-a1ab-d14b26331b0e"}
```

```json
{"id":"spaces/SPACE/messages/REPLY"}
```

Repeated sends with the same request ID must return the same message resource.
Do not silently fall back to a new thread or another space. A transport adapter can
wrap an organizational Chat client without putting private endpoints or credentials
in this package. The public REST adapter uses `https://chat.googleapis.com/v1/`.

## Scope

Each bridge connects one configured space to one coordinator. It handles text
messages and explicit replies, including progress updates. It does not discover trigger keywords in
other spaces, forward attachments, or supervise the coordinator's lifetime.
Keep the input composer empty between automated
deliveries; direct human input and bridge input need coordination.
