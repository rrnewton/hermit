# agentctl inbox reference

## The problem

A coordinator agent dispatches work to several persistent workers. Native subagents of a harness
report back automatically when they finish; persistent workers in their own terminals or hosted
sessions do not. Without help, the coordinator either polls every worker's output on a timer or
lets workers type into its session directly, which is unauthenticated, lands in the middle of its
work, and turns five simultaneous reports into five interruptions.

`agentctl inbox` is the queue between them. Notices are posted by workers, or by a watcher acting
for them. They are coalesced, and then delivered to the coordinator as one batch, in priority order.

## Dependencies

None for `post`, `list`, `render` and `deliver --via print`: they only read and write files under
the registry. `deliver --via agentcloud-notify` runs `agentcloudctl` (override with the global
`--agentcloudctl-bin`). `watch` runs `herdr` and, when present, `claude`.

## Layout

Everything lives under `<registry>/.inbox/<coordinator>/` (the registry is the global
`--registry`, default `.agentctl`):

| Path | Meaning |
|---|---|
| `live/<created-ms>-<id>.json` | one queued notice |
| `claimed/<claim-ms>-<batch>.json` | a batch handed to an adapter whose outcome is unknown |
| `delivered/<claim-ms>-<batch>.json` | delivered batches; the newest 200 are kept, and an unreadable one is skipped rather than blocking posts or deliveries |
| `released/<claim-ms>-<batch>.json` | claimed batches given up with `release` |
| `.lock` | held for every change to the queue |
| `.deliver.lock` | held for a whole delivery; a second concurrent delivery exits 75 |

Coordinator (`--to`) and worker (`--from`) names are 1-64 lowercase letters, digits, dots,
underscores or hyphens, starting with a letter or digit.

## Notice kinds, priority and coalescing

| Kind | Priority | Meaning |
|---|---|---|
| `blocked` | 1 | waiting on a permission dialog, a question, or other outside input |
| `exited` | 1 | the worker's session or pane ended |
| `idle` | 2 | finished a turn and waiting for input |
| `message` | 2 | an explicit message from the worker |
| `still-idle` | 3 | reminder that an already-reported idle worker is still idle |
| `progress` | 3 | new output while the worker keeps working |
| `working` | — | the worker resumed; stores nothing |

- **One live state notice per worker.** `blocked`, `exited`, `idle`, `still-idle` and `progress`
  are state notices. Posting one replaces the worker's current live state notice, keeps the
  original creation time, counts the replacement, and widens the transcript range when both name
  the same file. The latest state is the true one, but a replacement **never lowers the
  priority** of a notice the coordinator has not seen: an undelivered `blocked` followed by
  `progress` is still delivered at priority 1, and an undelivered `idle` followed by `still-idle`
  keeps priority 2 and so does not expire.
- **`working` withdraws** the worker's live state notice: the worker picked up work on its own,
  so the coordinator does not need to hear that it was idle. Messages stay.
- **Messages are never coalesced.** Give `--id KEY` to make a repeated post a no-op that prints
  the id of the notice already holding that key. The key is recognized while the notice is
  queued, claimed, or in one of the 200 kept delivered batches; after its batch record is pruned
  the same key would be queued again.
- **Stale reminders expire.** An undelivered priority-3 notice older than `--stale-after`
  (86400 seconds by default) is dropped when the queue is read. Priority 1 and 2 notices never
  expire.
- **Capacity.** A queue holds at most `--max-live` notices (200 by default). Stale notices are
  expired before counting. A post that would add a notice beyond the limit exits 75; a post that
  replaces an existing state notice is still accepted.
- Notice text is at most 4000 bytes. `--cursor PATH:START:END` records the transcript byte range
  the notice covers, so the coordinator can read the source instead of trusting a summary. PATH
  is at most 1024 bytes and may not contain control characters.

## Ordering and rendering

Delivery order is priority, then creation time. A batch starts with one header line, followed by
one block per notice. Each block shows the kind, worker, UTC date and time, text cut at 600
bytes, and the transcript range. Control characters other than newline and tab, and the Unicode
line and paragraph separators, are shown as U+FFFD, and every text line is indented, so a
notice cannot forge another notice's header.
Notices are added while the whole batch, header and trailer included, stays within `--max-bytes`
(3000 bytes by default). The first notice is always included, so one large notice cannot stall
the queue; a notice renders to at most about 3.5 KB. Notices that do not fit stay queued, and the
batch ends with a line saying how many.

`render` prints the next batch without claiming anything. `list` prints the queue as JSON.

## Delivery

`deliver` holds `.deliver.lock` for its whole run and follows these steps:

1. Live copies of notices that a claimed or delivered batch already holds are removed. A crash
   between writing a claim and removing its live files leaves such copies behind.
2. If a claimed batch exists from an earlier failed attempt, it is re-sent first, unchanged, but
   only to the adapter and session it was claimed for. Any other `--via` or `--session` exits 2
   until the batch is delivered or released.
3. Otherwise the next batch is rendered, written to `claimed/`, and its notices leave `live/`.
   The batch id is derived from the coordinator, the adapter and session, and the ids of the
   notices it carries, so a retry to the same session reuses its idempotency key and a delivery
   to a different session never does.
4. The adapter runs. On success the batch moves to `delivered/`. On failure it stays in
   `claimed/`, `deliver` exits 69, and the next `deliver` re-sends the same batch.

Adapters (`--via`):

- `print` writes the batch to stdout and counts that as delivered; an empty queue prints nothing.
  It is at-least-once: if the batch record cannot be written after printing, the next `deliver`
  prints the same batch again. Use it from a cron job, a harness loop, or any reader that
  consumes stdout.
- `agentcloud-notify` runs
  `agentcloudctl notify --session ID --mode cli-script --idempotency-key agentctl-inbox-<coordinator>-<batch> --text BATCH`
  and prints `{"delivered": N}` (N is 0 for an empty queue). The session receives the batch as a
  peer message from `cli-script`, even in the middle of a turn. The batch budget is capped at
  100000 bytes whatever `--max-bytes` says. A notify that runs longer than `--notify-timeout`
  (120 seconds by default) is killed and counts as a failure. Because a retried batch keeps its
  id and its session, a retry after an ambiguous failure reaches agentcloud with the same key and
  is delivered once.

`release --batch ID` gives up on a claimed batch that can no longer reach its adapter. Without
`--requeue` the batch moves to `released/` and its notices are not delivered. With `--requeue`
its notices return to the queue, even beyond `--max-live`. A requeued state notice is folded
into its worker's newer live state notice, which takes the higher of the two priorities. A
requeued batch reuses the old key only when the next batch goes to the same session and carries
exactly the same notices; then agentcloud delivers it at most once. Otherwise (another adapter
or session, a notice posted meanwhile, or a merged state notice) it goes out under a new key and
may reach the coordinator twice if the failed attempt actually landed.

## Watching workers

`agentctl inbox watch --to COORDINATOR` produces notices automatically from the workers visible
in Herdr. It only posts; delivery stays with `deliver`, so the two can run on different
schedules. It needs `herdr` (global `--herdr-bin`) and, for Claude Code workers, `claude`
(`--claude-bin`).

Each sample takes these steps:

1. `herdr agent list` names every pane that hosts an agent. Panes without an agent, the
   coordinator's own pane (a worker named like `--to`), and every `--exclude NAME` are skipped. A
   pane whose Herdr name is not a valid inbox name is called `pane-<pane id>`.
2. `claude agents --json` reports `busy` or `idle` for every interactive Claude Code session with
   its process id. `HERDR_PANE_ID` in `<proc-root>/<pid>/environ` names the pane; if two sessions
   name one pane, the lower process id wins. For those panes Claude's state is used, because
   Herdr's screen rules can show a working Claude pane as idle. Herdr's `blocked` still wins,
   because a permission dialog is visible on screen. If `claude` cannot be run or answers
   something unreadable, a pane Herdr identifies as Claude has an unknown state for that sample:
   nothing about it changes, and nothing is posted for it.
3. Other panes, and Claude sessions reporting a status other than `busy` or `idle`, use Herdr's
   state. `idle` or `done` must hold for `--idle-samples` samples in a row (2 by default) before it
   counts, so a brief pause between tool calls is not announced. Herdr's `unknown` is unknown.
4. When two panes would get the same worker name, the pane that already owns the worker record
   of that name keeps it, and every other one is renamed `<name>-<pane id>`, with a counter if
   that still collides.

Transitions become notices:

| Transition | Notice |
|---|---|
| to blocked | `blocked` |
| to idle (after the required samples) | `idle`, which replaces a standing blocked or exited notice |
| back to working while an idle, blocked or exited notice stands | `working`, which withdraws it |
| still idle `--remind-minutes` after the idle notice (30 by default), then twice and four times as long | `still-idle`, at most three times |
| missing from Herdr's list for `--exit-samples` samples in a row (3 by default) | `exited` |
| back in the list after `exited` | `working`, `idle` or `blocked`, by its state |

An `exited` notice says when the worker was last listed and how long before the notice that was
(in seconds under two minutes, otherwise in minutes), because the absence rule makes it arrive
`--exit-samples` intervals late.

**Parked workers.** `agentctl inbox park --to COORDINATOR --worker NAME` marks a worker parked:
for example, one whose goal is paused on purpose. `--off` unparks it, and without `--worker` the
command only prints the parked set and writes nothing. The set lives in `parked.json` in the inbox
directory, and the watcher rereads it on every sample. Any valid name can be parked, including one
that is not a worker yet; entries stay until unparked. For a parked worker:
- idle periods are not announced and earn no still-idle reminders;
- `blocked`, `exited` and `working` are still posted;
- an idle notice still replaces a standing blocked or exited notice.

An idle period that began while the worker was parked stays unannounced after it is unparked,
until the worker works and goes idle again. An idle notice that was already posted keeps its
reminder schedule: after unparking, a reminder that came due while parked is posted at the next
sample. For the watcher an unreadable `parked.json` parks nobody, which only makes it noisier.
`park` refuses with exit 1, and leaves the file untouched, when `parked.json` is unreadable or has
a schema other than 1.

An unconfirmed one-sample idle leaves a standing notice in place, so a blocked notice is still
withdrawn when the worker resumes. A sample in which Herdr lists no pane at all is treated as
unknown, not as every worker leaving; this is judged on Herdr's whole list, so the last worker
leaving while the coordinator's pane remains is still announced. An exited worker is remembered for a day. Removing a worker
from view with a new `--exclude` counts as it leaving.

A worker seen for the first time is recorded without a notice unless it is blocked, so starting
the watcher does not announce every idle worker at once; an idle period already under way when
it is first seen earns no reminders either. A worker is first recorded only once its state is
known. If `watch.json` is unreadable, the watcher starts
over this way, so workers that left in the meantime are not announced.

An `idle` notice carries the last assistant message the worker wrote to its Claude transcript
(`<claude-projects>/<project>/<session-id>.jsonl`, found from the session id) since its previous
notice. The byte range is recorded as the notice's cursor. At most the last 8 MiB of that range
are read, and a partial final line is left for the next notice. Without a transcript, and for
`blocked`, the notice carries the last 12 non-empty terminal lines above the pane's bottom six
rows.

State lives in `watch.json` under the inbox directory. A worker's new state is saved only after
its notice was posted: if a post fails (for example, the queue is full), that worker and every
worker not yet posted keep their previous state, so the next sample decides the same notices
again rather than losing them. Delivery of a notice is at-least-once: if the watcher stops after
a post but before saving, the next sample posts the same notice again, which the inbox merges
into the worker's live notice unless a delivery ran in between. One
watcher runs per coordinator (`.watch.lock`; a second exits 75). `--once` takes one sample,
posts, saves, and exits, for use from a cron job or a harness loop; otherwise it samples every
`--interval` seconds (30 by default). Each sample prints one JSON object: its time, the number of
workers seen, and each posted notice with its outcome. In continuous mode a failed sample prints
`{"error": ...}`, reports on stderr, and the watcher tries again at the next interval. Each
`herdr` or `claude` query is killed with its whole process group after 60 seconds, or two
seconds after the query process exits if a descendant still holds its output open.

`watch` exits 0 after a `--once` sample, 1 when its files cannot be read or written, 2 for
invalid arguments, 69 when `--once` cannot query Herdr, and 75 when another watcher holds the
lock.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | success (including an empty delivery and a duplicate post) |
| 1 | a file could not be read or written, or a record is corrupt |
| 2 | invalid arguments, or a retry that names a different adapter or session than the claimed batch |
| 69 | the delivery adapter failed; the batch stays claimed for a retry |
| 75 | busy: the queue is full, or another delivery is running |

## Examples

```bash
agentctl inbox post --to coord --from builder --kind progress --text 'tests 40/120' \
  --cursor /home/me/.claude/projects/p/abc.jsonl:1048576:1200000
agentctl inbox post --to coord --from builder --kind idle --text 'All 120 tests pass; pushed 3f2a9c1'
agentctl inbox post --to coord --from builder --kind message --id builder-final --text-file report.md
agentctl inbox list --to coord
agentctl inbox render --to coord --max-bytes 2000
agentctl inbox deliver --to coord --via print
agentctl inbox release --to coord --batch 5e0c7a9d31f2b8a4 --requeue
```
