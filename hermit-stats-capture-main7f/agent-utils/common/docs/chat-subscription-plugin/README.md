# Chat subscription process protocol

This Rust library carries ordered inbound chat subscriptions across a process
boundary without exposing a dynamic-library ABI. Frames are JSON preceded by a
four-byte big-endian length, bounded before allocation, and negotiated with an
explicit protocol version.

The host sends routing authority and an optional durable provider cursor. The
start frame may also carry a bounded, schema-identified JSON object containing
operator-controlled non-secret backend configuration. The manifest cannot
populate it or request credentials. The backend emits at most one
receipt-bearing batch before waiting for the exact
commit. The host sends that commit only after all child events and the provider
cursor are durable. Heartbeats remain ordered but require no durable commit.

Incoming JSON rejects duplicate object keys at every depth. Batch, frame, text,
identifier, payload, and diagnostic sizes are bounded. The protocol contains no
credentials, and process discovery or credential setup remains the host's
responsibility. A host that supplies credentials through the environment must
clear inherited state first and add only names from its operator-controlled
configuration. The manifest has no environment-selection field.

## Version 1 wire contract

Every frame is a nonzero four-byte big-endian unsigned payload length followed
by exactly that many UTF-8 JSON bytes. The payload limit is 1,048,576 bytes.
Objects reject unknown or duplicate keys. The protocol family is
`agentctl-chat-subscription`; manifests negotiate numeric version `1`.

The host begins with:

```json
{"type":"hello","min_version":1,"max_version":1}
```

The plugin selects version 1 and reports capabilities:

```json
{
  "type": "hello",
  "version": 1,
  "capabilities": {
    "backend_name": "example",
    "replay": "cursor",
    "full_message_data": true,
    "max_uncommitted": 1,
    "event_kinds": ["message_created", "checkpoint", "gap", "heartbeat"]
  }
}
```

`replay` is `current_only` or `cursor`. Event kinds are any unique supported
subset, but `heartbeat` is required. `max_uncommitted` is a nonzero `u16`; a v1
Start always requests one. Either side may instead send an Error frame during
negotiation.

After Hello, the host sends Start or Close:

```json
{
  "type": "start",
  "channel_ids": ["channels/example"],
  "allowed_senders": ["users/operator"],
  "max_uncommitted": 1,
  "resume_from": "opaque-inclusive-cursor",
  "backend_configuration": {
    "schema": "example.workspace.v1",
    "data": {
      "subscription": "subscriptions/example"
    }
  }
}
```

`resume_from` and `backend_configuration` may be `null`; a missing optional
field also means `null`. Configuration is an operator-controlled, non-secret
object. Its schema is a lowercase dotted slug. The compact schema-plus-data
envelope is at most 65,536 bytes, nesting and pre-encoding node count are each
at most 64 and 65,536 respectively. Credentials never appear in Start.

Once Start is validated and the backend opens, the plugin sends:

```json
{"type":"subscribed"}
```

Subscribed carries no cursor and is not a durable checkpoint. The plugin then
sends ordered Item frames with nonzero contiguous `u64` sequence numbers:

```json
{
  "type": "item",
  "item": {
    "kind": "heartbeat",
    "sequence": 1
  }
}
```

```json
{
  "type": "item",
  "item": {
    "kind": "batch",
    "sequence": 2,
    "provider_cursor": "opaque-provider-position",
    "delivery_id": "live-receipt",
    "events": [
      {"kind": "checkpoint"},
      {
        "kind": "message_created",
        "channel_id": "channels/example",
        "message_id": "messages/example",
        "thread_id": "threads/example",
        "sender_id": "users/operator",
        "text": "",
        "created_at": "2026-01-01T00:00:00Z",
        "thread_reply": false,
        "provider_payload": {
          "schema": "example.message.v1",
          "data": {
            "name": "messages/example"
          }
        }
      },
      {"kind": "gap", "reason": "retention boundary"}
    ]
  }
}
```

Checkpoint has only `kind`. Gap has `kind` and nullable `reason`.
Message-created has exactly the fields shown; `provider_payload` is nullable,
but must be present when `full_message_data` is true. Empty normalized text is
valid for attachment-only messages. `created_at` is RFC 3339 with an explicit
offset or `Z`.

A heartbeat has no durable receipt and permits the next Item immediately. A
batch is one upstream acknowledgement unit. The plugin emits no later Item
until the host has durably admitted every child event and its provider cursor,
then sends the exact live receipt:

```json
{"type":"commit","sequence":2,"delivery_id":"live-receipt"}
```

After the backend acknowledges or advances its local position, the plugin
confirms:

```json
{"type":"committed","sequence":2}
```

`provider_cursor` is durable inclusive replay authority. `delivery_id` is only
an ephemeral receipt for the current connection; it must never be persisted as
a cursor. Close or EOF before Commit leaves the batch unacknowledged. The host
can send Close through the public subscription cancellation method:

```json
{"type":"close"}
```

Only an explicit End is graceful provider termination. Raw EOF without End is
a retryable failure and the host reconnects from its last durable cursor:

```json
{"type":"end"}
```

Errors have this exact shape:

```json
{"type":"error","code":"provider_unavailable","detail":"bounded diagnostic","retryable":true,"fatal":true}
```

Any v1 Error ends the current generation. `retryable` says reconnection from
the durable cursor may succeed; it never authorizes replay of an uncertain
Commit. Transport EOF/I/O, truncation, and an unconfirmed Commit are retryable;
malformed, oversized, out-of-order, or otherwise contract-invalid frames are
not. `fatal` says the sender cannot continue this connection.

Canonical domain limits are 32 channels, 256 allowed senders, 256 events and
524,288 JSON-encoded variable payload bytes per batch, 8,192-byte cursors and delivery IDs,
2,048-byte resource IDs, 256-byte sender IDs, 32,000-byte message text,
128-byte timestamps and schema/backend labels, 2,000-byte gap reasons and error
details, and 262,144 encoded bytes for each complete provider payload
`{"schema":...,"data":...}` envelope, with 64 nesting levels inside its data.
The batch accounting includes JSON string escaping. Even with maximum escaped
cursors, delivery IDs, event count, and structural overhead, a core-valid Item
therefore remains below the 1,048,576-byte frame bound.
With `resume_from:null`, the first batch must begin with Checkpoint or Gap so
the current head is durable before message delivery. A non-null cursor is
replayed inclusively; the durable host deduplicates by event identity.

Close is cooperative and remains reachable while provider `next_item` blocks
when a plugin opts into `serve_interruptible`. That API owns a dedicated,
independently cancellable framed-input reader: a Close received during a blocked
receive invokes the backend's terminal, sticky cancellation authority, then the
main server path calls ordinary `subscription.close()`. Close received during a
Commit is queued until that Commit finishes and is consumed before another
receive starts. Natural End cancels and joins the reader actor even if the host
keeps its write half open. The original generic `serve<R: Read, W: Write>` API
remains sequential and accepts readers that are neither cancellable nor Send;
it has no background reader to strand, but can observe Close only when it is
already reading a protocol control frame. Linux `serve_stdio` uses the
interruptible path; non-Linux `serve_stdio` retains the generic sequential path.
The host side uses a separately accessible, frame-serialized writer only while
its worker is provably blocked reading the next provider frame. Success for
that path requires the complete Close write, EOF observed by that exact
post-Close read, and status zero from the exact pidfd-owned child; natural End,
unsolicited EOF, or nonzero exit cannot be relabelled as graceful cancellation.

Linux process hosts use `process::ProcessPluginChild::connect` with explicit
`ProcessPhaseTimeouts`. The returned backend runs blocking pipe I/O on an owned
worker. Hello, Start, Commit, and Close have separate deadlines; exceeding one
kills the complete private process group, closes the remote pipe ends, joins the
worker, and reaps the leader. An eventfd wakes forced-cleanup I/O, so even a
descendant retaining a pipe cannot strand the worker. If the caller's single
absolute cleanup deadline expires, the same terminal timeout is cached.
The supervising chat host publishes its selected Hello-inclusive join budget
before `connect` can block, then installs the cancellation authority before
Start. Consequently a stop during Hello retains `Hello + Close + grace + 7`
seconds, while a connected generation retains `Close + grace + 7`; Start is
interruptible rather than additive. The final seven seconds comprise the
process supervisor's two-second forced-reap bound and five seconds for host
reconciliation and worker joins. An admitted outbound operation separately
retains its configured operation timeout, configured shutdown grace, and the
same seven-second reap/join margin. The service hard bound is the maximum of
the applicable provider window and that outbound window, plus service-manager
scheduling margin. At startup the host pins the complete selected provider
timeout tuple and refuses a later generation whose manifest differs until the
service restarts; the outer bound cannot silently expand. At the accepted
provider maxima the windows are 167 and 137 seconds, while the accepted
outbound maximum is 67 seconds, so a fixed 75-second bound is not generally
valid.
Before any supervisor is cloned, the host reserves one of 32 global cleanup
admissions. That admission follows the live child into a tracked cleanup thread
until the process is reaped and the protocol worker is joined. A stuck cleanup
therefore consumes one bounded slot, and saturation refuses later launches
before creating another process or worker. If a cleanup thread cannot be
created, its complete task and admission remain retained instead of panicking
or detaching resources, and later launch attempts retry that tracked task before
seeking a new admission. Drop does not start another untracked wait.
An unexpected post-preflight pidfd-reap failure likewise leaves the pidfd,
cleanup thread, JoinHandle, and admission owned. A later launch attempt is an
explicit retry event; there is no timer polling and no release of capacity while
zombie ownership remains uncertain. If the plugin cannot transfer its self-opened
pidfd, the host first kills the still-pinned process group and then permanently
retains the plugin `Child`, supervisor pidfd, and one admission. It makes no
identity-unsafe numeric `Child::kill` or `Child::wait` call; repeated failures
therefore fail closed at the same global bound without accumulating threads.

Process plugins are trusted same-user code. The private process group is a
cleanup and supervision mechanism, not a security-containment boundary: a
malicious plugin can call `setsid` and escape process-group signalling. Hosts
that run untrusted plugins must add an appropriate sandbox or cgroup boundary.

Normal shutdown gives semantic Close its configured deadline, then waits the
configured cooperative process-exit grace, kills the process group before
reaping the leader, and finally waits through a fixed two-second post-kill
bound. Linux pidfds and `ppoll` provide event-driven exit
notification without periodic wakeups. `clone3(CLONE_PIDFD)` atomically creates
a minimal supervisor that becomes and remains the private process-group leader;
the ordinary plugin process joins that group. Because this raw supervisor never
execs, it closes every inherited descriptor except its one-byte readiness pipe
and one-byte parent-release socket before the handshake, then closes the release
socket before its long-lived wait. It uses `close_range` when available and an
async-signal-safe `/proc/self/fd` enumeration fallback otherwise; if neither
works, launch is refused before the plugin executable starts. Around `clone3`,
the launch thread uses raw `rt_sigprocmask` to block the complete Linux kernel
signal set and then restores its exact prior mask. The sentinel keeps every
blockable signal masked forever, so inherited handlers and group-directed signals
cannot run during raw post-clone setup or terminate it; cleanup uses SIGKILL.
Until the parent has restored its exact mask and wrapped the atomic pidfd in
tracked cleanup ownership, the sentinel also arms `PR_SET_PDEATHSIG(SIGKILL)` and
waits for a one-byte release handshake. Fatal restoration failure therefore
cannot orphan the raw child even if an external policy denies the parent's
best-effort pidfd signal. The supervisor pins the PGID until the host has signalled
the complete group, so a concurrent disposition change or external reaper cannot
redirect cleanup through PID/PGID reuse. Before exec, the plugin child opens a
pidfd for itself and transfers it to the host with `SCM_RIGHTS`; even a fast exit
or concurrent reaper therefore cannot make the host look up a reused numeric PID. The host preflights
`waitid(P_PIDFD)` as well as exercising the live clone3 pidfd before starting the
executable, so kernels that implement only the earlier clone3 primitive are
refused safely. There is deliberately no numeric-PID wait fallback: reap is
exclusively `waitid(P_PIDFD)`, interrupted calls retry, and no wait error alone is
treated as success. A signal-zero operation on the stable pidfd must return
`ESRCH` before an external status consumption is accepted; a consumed plugin
status is reported as unavailable rather than fabricated. Launch also refuses
an already-incompatible SIGCHLD disposition.
Non-Linux builds expose the same configuration types but refuse process launch
as unsupported before executing a command.
