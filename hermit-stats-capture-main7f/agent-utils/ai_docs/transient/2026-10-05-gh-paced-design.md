# gh-paced: design note (2026-10-05)

Working design record for `rs/gh-paced`. The settled user documentation is in
`common/docs/gh-paced/` (`QUICKSTART.md`, `USER_GUIDE.md`, `RELATED_WORK.md`),
and the binary prints it with `gh-paced quickstart` and `gh-paced userguide`.
This note records why the design looks the way it does, the evidence behind the
default budgets, the incident replay, and where the implementation departs from
the original request.

## Background and terms

- **gh** is the GitHub CLI. Each `gh` invocation makes one or more HTTPS
  requests to GitHub's REST or GraphQL API. gh has no client-side pacing: a loop
  over `gh` runs as fast as GitHub answers.
- **Primary rate limit**: GitHub's per-account hourly allowance, 5,000 REST
  requests and 5,000 GraphQL points per hour for a user token. `GET /rate_limit`
  reports how much of it is left.
- **Secondary rate limits**: further limits GitHub applies to bursts, such as
  concurrent requests, points per minute, and "content-generating" requests
  (anything that creates content: comments, issues, reviews). Some of these are
  undisclosed.
- **Token bucket**: a counter that refills at a fixed rate up to a maximum (the
  burst). A call spends tokens; when the bucket is empty, the next call waits.
- **Cooldown**: a period in which gh-paced runs no calls at all for an account
  on a host, started when GitHub refuses or throttles a call.
- **Snapshot**: the last `GET /rate_limit` result, cached in the state file.

## Why it exists

On 2026-10-04 a publishing script, using the shared bot account, posted 22
comments to one pull request: a 4,048-byte summary and 21 request bodies that
each carried a chunk of a base64-encoded archive. It read each comment back
immediately after posting it. Its own logs show 44 calls (22 POSTs and 22 GETs)
starting within 38.5 s, from 11:04:12.356 to 11:04:50.841 ET; the 22 POSTs
themselves took 37.49 s. The final read-back failed with GitHub's account-wide
"API rate limit exceeded" error, because other polling on the same account had
already used the hour's allowance. GitHub suspended the bot account shortly
afterwards. A suspended account's issues, pull requests and comments are hidden.

The owner's personal, long-term account is what remains, and losing it is not
recoverable. The owner asked for a wrapper that paces gh on the client side,
prints loud warnings when it does, and keeps two to four machines sharing one
account under GitHub's limits.

Corrections to earlier informal accounts of the incident: it was 44 calls, not
43; it lasted 38.5 s, not about 60 s; and the part request files were 60,601 to
60,602 bytes (60,000-character base64 lines), with a last part of 30,982 bytes
(a 30,380-character line), not "60.5 KB each".

## Design summary

**Placement.** gh-paced is a release-optimised Rust binary in the `rs/`
workspace. An account shim (a script that selects the account's credentials and
then runs gh) replaces its final `exec <real gh> "$@"` with
`exec gh-paced --account <name> --real-gh <real gh> -- "$@"`. gh-paced runs the
real gh as a child and returns its exit status (or dies by its signal). stdin,
stdout and stderr pass through; if stderr is a terminal the child gets a
pseudo-terminal for stderr, so gh still sees a TTY while gh-paced scans it.
A teed stream (stderr, and stdout for `api --include`) is scanned before it is
queued for the consumer, and every byte read is delivered however slowly the
consumer reads; only an INT, TERM, HUP or QUIT after gh has exited (sent, or
typed at the terminal) abandons undelivered output, and gh-paced then dies by
that signal within about 2 s even if the consumer has stopped reading. A
pushback signal is recorded as soon as the reader sees it, before the chunk
that carried it is queued.

**Classification.** Each command line is classified without touching the
network:

| Class | Examples | Cost |
| --- | --- | --- |
| LOCAL | `--help` after a known gh command, `version`, `completion`, gh's own `config` and `alias` subcommands, `auth git-credential store/erase` | not paced, not audited |
| READ | `pr view`, `issue list`, `run view`, `api` GET, GraphQL `query`, `status` | 1; `--paginate` 10; `--limit N` ceil(N/100); `status` 10 |
| SEARCH | `search ...`, `api search/...`, `extension search` | 1 |
| WRITE | `pr create`, `issue comment`, `api -X POST`, GraphQL `mutation`, anything unknown (even with `--help`), an unknown word beneath a group, `extension exec`, `copilot` | 1; `--paginate` 10 |
| GIT_CREDENTIAL | `auth git-credential get` | 1 |

A call costing more than its class's burst is refused before gh runs (exit 75),
except a watch, because one gh call makes its requests back to back.

gh's own aliases are expanded first, the way gh 2.97 expands them, and the
expansion is what is classified, guarded and passed to gh (see round 5).
Unknown commands, extensions, `!` shell aliases and unreadable GraphQL queries
are WRITE.
`pr checks --watch` and `run watch` are refused (exit 64) unless
`GH_PACED_ALLOW_WATCH=1` is set; with it they cost 20 READ tokens. Every `--interval`
value must be a plain positive whole number of seconds, at least 30, or the call
is refused (exit 64). A watch is stopped (TERM, then KILL 5 s later; exit 75)
at `floor((cost - 2) / requests per poll) x interval` seconds: 2 tokens pay for
startup, and a poll is estimated at 4 requests for `run watch` and 2 for
`pr checks --watch`, so 120 s and 270 s at a 30 s interval. A cost above the
class's hourly cap is refused at once.

**State.** One JSON state file per account on each host
(`~/.local/state/gh-paced/<account>.json`), updated under an `flock` on a
separate `<account>.lock` and replaced atomically. The lock is never held across
a sleep or a network call, and the clock is read only after it is taken. A
write's in-flight slot is a lease: a file `<account>.lease-<nonce>` with an
exclusive `flock` whose open file gh inherits, so the slot lasts until gh and
everything holding that file exit, even if the wrapper is SIGKILLed. If the
wrapper finishes while a process gh started still holds the lease, the slot and
the file stay until that process exits. A nested gh-paced proves that it
descends from a holder only by holding the holder's locked open file, which
`/proc/self/fdinfo` shows (Linux lists a `flock` only for the open file that
took it), so reopening the lease file proves nothing. A process counts as alive
only while its PID and start time match and it is not a zombie. A state file
that cannot be parsed is hard-linked aside and replaced, by one atomic rename,
with a recovery state: every class blocked for 3,600 s (`blocked_until`, which
does not depend on the configured limits, so raising a limit afterwards does not
reopen the hour), a 900 s cooldown, and the still-locked leases restored. The
latest pushback cooldown is also kept in `<account>.cooldown` and merged on
every load, so a damaged state file cannot shorten a long `Retry-After` pause; a
damaged cooldown file triggers the same recovery.

**Admission.** A call is admitted when its bucket holds enough tokens (or is
full, for a call that costs more than the burst) and the cost fits the
class's sliding one-hour cap. The full cost is charged, so an expensive call
leaves the bucket in debt. Otherwise gh-paced computes the wait (the longest of
cooldown, account-wide block, free write slot, refill, and hourly cap), prints
one `GH-PACED WARNING` line on stderr with the next slot in US Eastern time, and
sleeps. If the time already slept plus the next wait exceeds
`GH_PACED_MAX_WAIT` (default 900 s), it refuses with exit 75 and runs nothing.

**Account-wide feedback.** gh-paced refreshes the snapshot when none exists,
when it is at least 300 s old, or after 50 admitted calls, at most once per 60 s
and by one process at a time, never during a cooldown. The refresh costs one
READ token, charged under the lock before the request is sent. When a refresh
is due and READ has no room for it, the call waits for room (and is refused past
`GH_PACED_MAX_WAIT`) rather than run without current account numbers. The
numbers are re-checked on every admission pass, so after any sleep. At 50% or
less
remaining (on `core`/`graphql`, plus `search` for SEARCH) the READ, SEARCH and
WRITE budgets are halved; at 20% or less those calls block until the resource
resets.

**Pushback.** gh's stderr is scanned with a 4 KiB overlap for `secondary rate
limit`, `rate limit`, `HTTP 429`, `abuse`, `submitted too quickly`,
`Retry-After: N`, and plain `HTTP 403`. For `api -i/--include`, stdout header
blocks are also read (status 403/429, `Retry-After`, `X-RateLimit-Remaining:
0`). A header block is acted on only in the shape gh prints it: a status line,
header lines ending in CRLF, and a CRLF blank line; a response body that merely
looks like headers (printed through `--jq`, say, with plain LF line ends) is
never matched. Without `--paginate` only the first stdout block is read (gh
prints exactly one, first), so no body can start a cooldown; with `--paginate`
every block is read. Any of them starts a cooldown of
max(Retry-After, 900 s), printed as a banner, and discards the snapshot. A plain
403 also gets 900 s, and neither cooldown can be configured lower.

**Content guard.** Before a WRITE runs, every body source is read: inline
flags, `--body-file`, `-F field=@file`, `--input FILE`, stdin, gist files,
release notes files. A JSON request body counts at its full size, and every
string inside it is also decoded and scanned. The call is refused (exit 65)
when the total exceeds 8,192 bytes or any source has a base64-looking run
longer than 1,000 characters. Any unbroken base64-alphabet run counts, hex
and rulers included, and the detector joins every block of consecutive lines
that are each at least 20 base64-alphabet characters, whatever they contain, so
line-wrapped encodings are caught; 26 or more bare 40-character SHAs, one per
line, therefore count as a run over 1,000. Every argument of an alias,
extension or unknown command, flag-shaped or not, is inspected as text. Body
files are first copied to a private directory,
`snap-<pid>-<start ticks>-<nonce>/`, holding a `.lock` file locked by the
invocation and inherited by gh; both the guard and gh read the copy, so a file
cannot change between the check and the send. The directory is removed when the
invocation ends, unless a process gh started still holds the lock. Every paced
invocation also sweeps directories whose lock is free, whose creator process
has exited, and that are at least 60 s old (any other `snap-*` name after 24 h).
Forms where gh composes the body itself after the guard (editor, template,
`--fill`, `release create --notes-from-tag`, interactive prompt, `gist edit`
without `--add`/`--remove`) are refused with exit 65; boolean flags are read for
their value, so `--web=false` is not the web form. The limits can only be
lowered by configuration. `GH_PACED_ALLOW_LARGE_BODY=1` skips the guard for one
call.

**Audit and status.** Each paced call appends a JSON line (time in UTC and ET,
host, PID, event, class, cost, command, redacted argv, waited seconds, detail)
to `<account>.audit.jsonl`. No bodies, tokens or environment are written; an
endpoint loses its host, userinfo, query string and fragment, an unknown
command keeps only its flag names, and an unknown flag of a known command keeps
its name but loses its value. `gh-paced status` prints the buckets,
in-flight writes, cooldown, snapshot and last 5 records from local files only,
without taking the lock or creating files.

## Budgets and the GitHub limits behind them

Defaults per host, per account:

| Class | Per minute | Burst | Per hour | Other |
| --- | --- | --- | --- | --- |
| READ | 20 | 10 | 500 | |
| SEARCH | 5 | 2 | 150 | |
| WRITE | 2 (1 per 30 s) | 1 | 30 | 1 in flight per host |
| GIT_CREDENTIAL | 6 (1 per 10 s) | 1 | 120 | |

GitHub's documented limits, quoted verbatim:

- [Rate limits for the REST API](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api):
  "All of these requests count towards your personal rate limit of 5,000
  requests per hour." "No more than 100 concurrent requests are allowed."
  "No more than 900 points per minute are allowed for REST API endpoints, and
  no more than 2,000 points per minute are allowed for the GraphQL API
  endpoint." "In general, no more than 80 content-generating requests per minute and no
  more than 500 content-generating requests per hour are allowed. Some
  endpoints have lower content creation limits." "These secondary rate limits
  are subject to change without notice." "If the retry-after response header is
  present, you should not retry your request until after that many seconds has
  elapsed. ... Otherwise, wait for at least one minute before retrying."
  "Continuing to make requests while you are rate limited may result in the
  banning of your integration."
- [Search](https://docs.github.com/en/rest/search/search): "30 requests per
  minute for all search endpoints except for the Search code endpoint."
- [GraphQL rate limits](https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api):
  "For users: 5,000 points per hour per user."
- [Rate limit endpoint](https://docs.github.com/en/rest/rate-limit/rate-limit):
  "Accessing this endpoint does not count against your REST API rate limit."

### Fleet arithmetic: four hosts on one account

| Budget | Four hosts at the default | GitHub's limit | Share |
| --- | --- | --- | --- |
| READ per hour | 4 x 500 = 2,000 calls | 5,000 requests | 40% |
| READ per minute | 4 x 20 = 80 | 900 points | 9% |
| SEARCH per minute | 4 x 5 = 20 | 30 | 67% |
| WRITE per minute | 4 x 2 = 8 | 80 content-generating | 10% |
| WRITE per hour | 4 x 30 = 120 | 500 content-generating | 24% |
| Writes in flight | 4 x 1 = 4 | 100 concurrent (all kinds) | 4% |

Caveats. A gh call can make more than one HTTP request; at two per call, four
hosts at their full READ budget make 4,000 requests an hour, 80% of the primary
limit. Calls that do not go through gh-paced (the web UI, Actions running as the
user, a script's own HTTP client) are invisible to the local budgets; the
snapshot is the only feedback that covers them. Between two refreshes, four
hosts can spend at most about 4 x 20 x 5 = 400 READ tokens (8% of 5,000), or
200 when halved, which is why the blocking floor is 20% rather than lower.

SEARCH is the tightest share (67%). It is still under the limit, and the
snapshot halves it when the search pool drops to 50%.

## Incident replay

All of these run in `cargo test -p gh-paced` (`tests/replay.rs`) with a fake gh
and a fake clock, so no network is used. The fake gh records each invocation at
the fake time; sleeps advance the fake clock. Numbers below are the test's own
`eprintln!` output from one run.

### (a) The content guard refuses the archive parts

`incident_parts_are_refused_and_the_summary_is_allowed` replays the original
sequence (summary, read-back, part, read-back, ...) with request files shaped
like the originals: a JSON object whose `body` holds one long base64 line. The
summary is allowed; all 21 parts are refused with exit 65 before gh runs:

```text
GH-PACED REFUSED [replay] content guard: write body is 60074 bytes across 1 source(s), over the 8192-byte limit; --input contains a base64-looking run of at least 8127 characters (limit 1000); never store encoded data in GitHub text
GH-PACED REFUSED [replay] not running `api POST repos/o/r/issues/1/comments` (exit 65)
```

The "at least" is because files are read only up to the limit plus one byte.
The run finished at fake time +39.6 s. gh ran 23 times (1 POST, the summary,
and 22 GETs) plus 1 rate-limit refresh. The READ window held 23 tokens (22 GETs
and the refresh) and the WRITE window 1.

The real incident request files were also run through the release binary,
read-only, with a fake gh and a scratch state directory:

| Binary | Configuration | Summary (4,048 B) | Parts 01-21 | Example refusal |
| --- | --- | --- | --- | --- |
| round 1 (uncommitted on `c06243e7`) | defaults | exit 0 | all 21 exit 65 | part 01: 60,601 bytes over 8,192, base64 run of at least 7,603 |
| round 1 | `max_body_bytes` 65,536 | | part 01 exit 78 | `max_body_bytes must be in 256..=8192, got 65536` |
| round 1 | `max_base64_run` 100,000 | | part 01 exit 78 | `max_base64_run must be in 100..=1000, got 100000` |
| `c06243e7` | `max_body_bytes` 65,536 | | 01 and 21 exit 65 | base64 runs of 60,001 and 30,381 |
| `c06243e7` | `max_base64_run` 100,000 | | 01 and 21 exit 65 | 60,601 and 30,982 bytes over 8,192 |

In the defaults row the fake gh's log shows one rate-limit refresh and one POST,
the summary, sent from the private copy
(`<state dir>/snap-<nonce>/0/SUMMARY.request.json`; since round 2 the directory
is named `snap-<pid>-<start ticks>-<nonce>`); no part reached it, and no copy
was left behind. The audit log of that run holds no base64-alphabet run of
100 characters or more. The round-1 binary refuses the two loosened
configurations outright (exit 78, the configuration floors), so the last two
rows, which show that each check alone refuses the parts, were measured with
the earlier binary, which accepted them. In those runs the fake gh's log file
was never created: a refused call reaches neither gh nor the rate-limit
refresh. The `guard` unit tests cover each check separately in the current
code.

### (b) Prose bodies are paced

`prose_bodies_are_spaced_30_seconds_apart_and_gets_use_the_read_bucket` replays
the same 44-call sequence with 4,096-byte prose bodies, which pass the guard:

- 22 POSTs ran from +0.3 s to +630.9 s: a span of 630.6 s, minimum gap
  30.000 s (the test requires at least 30 s less 1 microsecond). The requirement was at least 21 x 30 s = 630 s (10.5 min). The
  original sequence took 37.49 s for the same 22 POSTs.
- The first wait printed
  `GH-PACED WARNING [replay] write budget 1 per 30 s (burst 1) per host reached; sleeping 29 s`.
- Total sleep: 604.8 s. The 22 GETs ran from the READ bucket between the
  writes and were never charged as writes: the WRITE window held 22 tokens and
  the READ window 22 plus the 3 rate-limit refreshes.

`hourly_write_cap_holds` posts 31 writes in sequence. The first 30 run (one per
30 s). The 31st is refused under the default 900 s bound with
`write budget 30/hour per host reached (30 used in the last hour)`; with a
4,000 s bound it is admitted at +3,600.3 s, and no hour ever holds more than 30.

### (c) A depleted account blocks every API call

`low_account_budget_blocks_every_api_call_loudly` sets the snapshot to 900 of
5,000 core requests left (18%) with the reset 1,800 s away. A POST, a GET,
`pr view`, `search issues` and a GraphQL query are each refused with exit 75, and
gh is never started:

```text
GH-PACED WARNING [replay] account-wide core budget at 18% (900/5000 left until 11:34 AM ET); local read rates halved to 10/min and 250/hour
GH-PACED REFUSED [replay] account-wide core budget at 18% (900/5000 left), at or below the 20% floor; blocking until it resets
GH-PACED REFUSED [replay] the next slot is in 1800 s (at 11:34 AM ET), beyond GH_PACED_MAX_WAIT=900 s
GH-PACED REFUSED [replay] not running `api GET repos/o/r/issues/comments/1009` (exit 75)
```

A caller that allows a 3,600 s wait sleeps to the reset and then runs.
`half_account_budget_halves_local_rates` checks the 50% step: the READ budget
becomes 10/min and 250/hour, and the halved burst forces a wait.

Fleet read volume: four hosts at the default READ budget can make 4 x 500 =
2,000 paced gh calls an hour, 40% of the 5,000-request primary limit (80% if
every call made two requests). The incident's exhaustion came from traffic
outside any pacing; under gh-paced, the snapshot would have reported the low
remaining count and blocked these calls before the read-back that failed.

### (d) Where this lives

This section is the record. The tests above are part of `make validate`: the
step `gh-paced` / `test` in `validation/rust.dag.yaml` (reported as
`rust.gh-paced.test`) runs them, and two path rules in `scripts/validate.py`
select it. `rs/gh-paced/` selects the Rust gate and the Rust workspace tests;
`common/docs/gh-paced/` selects those plus the docs checks, because the binary
compiles its guides in with `include_str!`. gh-paced is deliberately not a
component in `validation/components.json`: every component there must name at
least one Python test file, and gh-paced has no Python side.

## The cooldown and the wait bound

After a pushback the cooldown is 900 s, and `GH_PACED_MAX_WAIT` also defaults to
900 s. A call made at the start of a fresh cooldown therefore needs a 900 s wait,
which is not more than the bound, so it **sleeps the full 15 minutes** and then
runs. It is refused only once the time already slept plus the remaining wait
exceeds the bound. This is by design: the request was "issue sleeps", and a
caller that would rather fail fast sets a smaller `GH_PACED_MAX_WAIT`. It does
mean an agent can appear to hang for 15 minutes after a pushback; the warning
line it prints first says so and gives the end time.

## Live smoke test

Run on 2026-10-05 against GitHub with the owner's personal account, through the
release binary, with a scratch state directory and a two-line `--real-gh` script
that calls the existing account shim. The shim itself was not modified.

| Time (ET) | Call | Result |
| --- | --- | --- |
| 03:49:23 | `api rate_limit` (no proxy) | exit 1: TCP connect failed ("network is unreachable"); nothing reached GitHub |
| 03:49:40 | `api rate_limit` | exit 0; core 5000/5000, graphql 5000/5000, search 30/30 |
| 03:49:56 | rate-limit refresh (`api rate_limit`, made by gh-paced) | parsed: core 5000/5000 left (100%) |
| 03:50:08 | `api repos/<owner>/agent-utils --jq .full_name` | exit 0, printed the repository name |

Three requests reached GitHub, 16 s and 12 s apart. The second call was run with
`GH_PACED_READ_PER_MINUTE=5 GH_PACED_READ_BURST=1`, so after the refresh took
the only READ token the GET had to wait. gh-paced printed
`GH-PACED WARNING [<account>] read budget 1 per 12 s (burst 1) per host reached; sleeping 12 s (next slot 3:50 AM ET)`
and the call took 12.8 s. `gh-paced status` afterwards showed the snapshot
(19 s old, core and graphql resetting at 4:49 AM ET) and the audit records
`exit`, `refresh`, `throttle`, `admit`, `exit`. The account had used none of its
primary allowance in that hour.

## Departures from the request, with reasons

1. **stdout is scanned only for `api --include` header blocks.** GitHub's
   errors arrive on gh's stderr; stdout carries the caller's data (`--json`,
   `gh api` bodies) that may legitimately contain "rate limit" or "abuse". For
   `api -i/--include`, whose status line and headers are on stdout, header
   blocks are read (403/429, `Retry-After`, `X-RateLimit-Remaining: 0`) and
   bodies are not. (Round 1 found that a `Retry-After: 3600` there was missed.)
   A header block counts only in the shape gh prints it: header lines and the
   closing blank line end in CRLF, while jq output uses LF. (Round 2 found that a
   `--jq .body` output holding a status line and `Retry-After: 99999` started a
   false cooldown.) Without `--paginate` only the first block is read, and a
   block cut off by the end of the output still counts. (Round 3 found that a
   body with CRLF header lines started a false cooldown, and that a cut-off
   block lost its `Retry-After`.)
2. **GraphQL is classified by its query.** `api graphql` with a readable
   `query` is READ; a `mutation`, or a query that cannot be read (from a file
   gh-paced cannot open, say), is WRITE. Treating every GraphQL call as WRITE
   would make read-only tooling unusable at one call per 30 s.
3. **List and view subcommands are split from writes** per command family
   (`label list` READ, `label create` WRITE; `gist view` READ, `gist create`
   WRITE; `extension list` READ, `extension install` WRITE).
4. **`auth git-credential store` and `erase` are LOCAL.** Only `get` returns a
   credential for a network operation; git calls `store`/`erase` after it, and
   they do not contact GitHub.
5. **Update notifiers are disabled for the child.** gh-paced sets
   `GH_NO_UPDATE_NOTIFIER=1` and `GH_NO_EXTENSION_UPDATE_NOTIFIER=1` unless the
   caller set them, because the release check is an unpaced request.
6. **Watch loops are charged, bounded, and fast ones refused.** `pr checks
   --watch` and `run watch` cost 20 tokens and are refused (exit 64) below a
   30 s interval unless `GH_PACED_ALLOW_FAST_WATCH=1`. Every `--interval`
   value must be a plain positive decimal integer; zero, a negative value, or a
   form such as `0x1e` is refused even with the override, because gh treats
   zero or a negative interval as no sleep at all. A watch is stopped at
   `floor((cost - 2) / requests per poll) x interval` seconds (TERM, KILL
   after 5 s, exit 75), with 2 tokens for startup and an estimated 4 requests
   per `run watch` poll (run, workflow, jobs, and a margin) and 2 per
   `pr checks --watch` poll: 120 s and 270 s at 30 s. (Round 1 found the
   earlier unbounded watch; round 2 found that `--interval -1` after a valid
   value was ignored by gh-paced but honoured by gh, and that 2 requests per
   `run watch` poll was too few.)
7. **`--limit N` costs ceil(N/100) tokens** on list and search commands, the
   number of 100-item pages it may fetch.
8. **`gh status` is READ with cost 10.** It makes several GraphQL and REST
   read requests; its GraphQL search draws on GraphQL points, not the REST
   search limit. (It was SEARCH with cost 3 until round 4's burst refusal made
   it unrunnable at the SEARCH burst of 2; see round 4.)
9. **Environment variables only tighten** a budget; a looser value is ignored
   with a warning. `GH_PACED_MAX_WAIT` and `GH_PACED_LOCK_WAIT` are the
   exceptions: they set how long the caller will wait, not how fast GitHub is
   called. Config-file values are
   clamped to GitHub's documented ceilings.
10. **stderr goes through a pseudo-terminal when it is a terminal**, so gh keeps
    its interactive behaviour while gh-paced scans the stream.
11. **A plain HTTP 403 has its own cooldown setting**
    (`plain_403_cooldown_secs`, default 900 s, as requested), but it has the
    same 900 s floor as the rate-limit cooldown. (Round 1 found it could be set
    to zero.)
12. **Exit statuses** beyond 75: 65 for the content guard, 64 for usage errors,
    70 for state errors, 78 for configuration errors (including a `--real-gh`
    that resolves to gh-paced), 127 when the real gh is missing.
13. **A corrupt state file is quarantined** to `<account>.json.corrupt-<secs>`
    (a hard link, with `-1`, `-2`, ... appended if that name exists) and
    replaced, by one atomic rename, with a recovery state: every class blocked
    for 3,600 s, a `cooldown_secs` pause, and every still-locked write lease
    restored. The block is a time (`blocked_until`), not a full window, so it
    does not shrink when a limit is raised later. The account is paused on that
    host for an hour. (Round 1 showed that the earlier empty buckets could
    reopen an exhausted hourly budget or erase a cooldown; round 2 showed that a
    crash between moving the file and saving the recovery state, or removing a
    tightened `GH_PACED_WRITE_PER_HOUR` after recovery, reopened the account
    early.)
14. **LOCAL calls are not audited**, to keep the log about GitHub traffic.
15. **GIT_CREDENTIAL is not blocked by the snapshot.** git operations do not use
    the API pools the snapshot reports; they keep their own bucket and the
    pushback cooldown still applies.
16. **Nested gh-paced** (an extension calling gh) skips its own parent's write
    slot only when it proves descent twice: the parent's nonce is in
    `GH_PACED_INFLIGHT_CHAIN`, and it holds the parent's locked open file, shown
    by a `flock` line with `WRITE` access in `/proc/self/fdinfo/<fd>`. Linux
    lists that line only for the open file description that took the lock. It
    refuses past 8 levels (exit 75). (Round 1 found the chain variable alone
    could be forged; round 2 found that redirecting stdin from the lease file
    could too.)
17. **Configuration cannot weaken the safety floors.** Cooldowns are at least
    900 s, blocking starts at 20% or higher, halving at 50% or higher, the body
    limit is at most 8,192 bytes and the base64 limit at most 1,000 characters,
    and the snapshot is refreshed at least every 300 s and 50 calls. A value
    outside its range exits 78. Only `GH_PACED_ALLOW_LARGE_BODY=1`, per call,
    loosens the guard.
18. **Writes whose body gh composes later are refused** (exit 65): editor,
    template, `--fill`, interactive prompts on a terminal, and `gist edit`
    without `--add`/`--remove`. The guard cannot see those bodies.
19. **Bare SHA lists count as base64.** Every block of consecutive lines that
    are each at least 20 base64-alphabet characters is counted as one run,
    whatever it contains, so 26 or more bare 40-character SHAs, one per line,
    exceed the 1,000-character limit. The request named only base64, but the
    detector cannot tell a digest list from a hex or lower-case encoding (round
    2 showed `aaaa` repeated 300 times, wrapped at 76 columns, passing). The
    same SHAs with a subject on each line are prose and count 40.
20. **`release create --notes-from-tag` is refused** (exit 65), like the other
    forms whose body gh composes: gh reads the tag annotation or commit message
    after the guard and sends it as the release body.
21. **Boolean flags are read for their value.** `--web=false`, `--editor=false`
    and `--fill=false` mean what they mean to gh, both for the refusals and for
    the web exemption.
22. **Every argument of an alias, extension or unknown command is inspected as
    body text**, flag-shaped or not (`--payload=<text>`, `--repo <text>`,
    `-- <text>`), and every occurrence of a repeated flag rather than only the
    last, because the expansion may pass any of them to a body flag. (Round 3
    found the last-occurrence pruning still applied to aliases.)
23. **The audit log drops the values of unknown flags.** `api repos/o/r
    --token=X` is recorded as `api <arg> --token`; the request asked for no
    secrets in the log, and gh-paced cannot know what an unrecognised flag
    carries. For the same reason a `gh api` call with an unrecognised flag has
    the command `api <unparsed>`, since its endpoint cannot be told apart from
    the flag's value. (Round 3 found `api GET CANARY` recorded for
    `api --token CANARY repos/o/r`.)
24. **A due refresh waits for READ room.** When the snapshot is due and READ
    has no token for the refresh, READ, SEARCH and WRITE calls wait (or are
    refused past `GH_PACED_MAX_WAIT`). Before round 2 the refresh was skipped
    and, after a pushback had discarded the snapshot, a WRITE or SEARCH could
    run with no account-wide numbers at all.
25. **The cooldown is also kept in its own file** (`<account>.cooldown`), merged
    on every load, so a damaged state file cannot shorten a long `Retry-After`
    pause. The request named one state file.
26. **A finished write keeps its slot while a descendant holds the lease.** gh
    may leave a background process holding the inherited lease. The wrapper
    then closes its own copy and leaves the holder record and lease file in
    place; a later caller reaps them once the lock is free. Before round 2 the
    slot was released while that process could still be writing.
27. **Zombie processes count as dead.** A process whose PID and start time match
    but whose state is `Z` or `X` no longer holds a refresh claim or a holder
    record, so a killed, unreaped refresher cannot block callers indefinitely.
28. **Every byte read from gh is delivered; reading after gh exits is
    bounded.** After gh exits, the reader stops at end of file, after 2 s of
    silence, 5 s after the exit, or after 64 MiB more (a background process may
    hold the stream open), and the writer is then waited for without a time
    limit. Output a background process writes after that cutoff is lost (a
    round-3 known gap). Before round 2 a slow consumer lost
    everything not delivered within 2 s (a mutant with the old cutoff delivered
    65,536 of 122,935 bytes).
29. **Snapshot directories are swept by ownership, not age alone.** See the
    content-guard summary above. Before round 2 the sweep ran only for calls
    that copied files, and could delete a live copy after 24 h.

## Review round 1 and the fixes

Codex (gpt-6.1-sol, reasoning effort ultra, read-only sandbox) reviewed
`fb96152e..c06243e7` and requested changes: 1 blocker, 15 major, 2 minor. Each
finding, its fix, and the test that fails without the fix:

| # | Finding | Fix | Test |
| --- | --- | --- | --- |
| 1 | GraphQL `-f query=` preferred over `--input` | every candidate document is inspected; `--input -` is WRITE and its stdin is buffered and replayed | `classify::graphql_*`, `cli::stdin_reaches_gh_untouched` |
| 2 | unknown command with `--help` was LOCAL | `--help` is LOCAL only after a known gh command | `classify` unit tests (`my-alias --help` is WRITE) |
| 3 | bodies gh composes later escaped the guard | `guard::uninspectable` refuses them (exit 65); implicit `gist create` stdin is a source | `guard` unit tests |
| 4 | files re-read by gh after the guard | private file copy (`snapshot.rs`); gh gets the copy | `snapshot` unit tests |
| 5 | first occurrence of a repeated flag used | costs take the most conservative value; the guard keeps the last occurrence, as gh does | `classify`, `guard` unit tests |
| 6 | base64 detection required a digit | any unbroken base64-alphabet run counts; multi-line blocks need upper case, `+` or `/` | `guard::base64_detection` |
| 7 | paginated write cost 1 | costs `paginate_cost` (10) | `classify::paginate_and_limit_costs` |
| 8 | unbounded watch | deadline from the purchased polls | `cli::watch_past_its_deadline_is_stopped` |
| 9 | cost above the hourly cap admitted | refused at once (infinite wait) | `budget` unit test |
| 10 | clock read before the lock | clock read after `flock` | `replay::time_is_read_after_the_lock_is_taken` |
| 11 | in-flight slot tied to the wrapper PID | lease file lock inherited by gh; nested exclusion needs the inherited fd | `cli::killed_wrapper_leaves_its_orphaned_gh_holding_the_write_slot`, `cli::nesting_needs_the_inherited_lease_not_just_the_chain` |
| 12 | corrupt state reset to empty buckets | full hourly windows, cooldown, restored leases | `state` unit test, `replay::corrupt_state_pauses_the_account_for_an_hour` |
| 13 | refresh charged after it ran; not rechecked after sleeps | claimed and charged inside the admission loop | `replay::refresh_requests_are_charged_before_they_are_sent`, `replay::feedback_is_rechecked_after_a_sleep` |
| 14 | `--include` headers on stdout ignored | stdout header blocks scanned for `api -i` | `cli::include_headers_on_stdout_set_the_cooldown`, `pushback` unit tests |
| 15 | configuration could weaken protections | floors in `config::floors`, out-of-range is exit 78 | `config` unit tests |
| 16 | audit kept query strings and alias arguments | endpoint normalised; unknown commands keep flag names only | `cli::query_string_secrets_stay_out_of_the_records`, `guard` unit tests |
| 17 | in-flight wait warned every 30 s | polls every 5 s and warns on every poll | `replay::in_flight_wait_warns_on_every_poll`, `cli::only_one_write_in_flight_per_account` |
| 18 | `status` took the lock and created files | reads the atomically replaced file without locking | `cli::status_takes_no_lock_and_creates_nothing` |

Seven of these tests were also checked by mutation: re-introducing the old
behaviour (nested exclusion on the chain alone, clock before the lock, no stdout
scan, no deadline, the orphan check by PID only, `status` taking the lock, and a
warning only on the first in-flight check) makes the named test fail.

## Review round 2 and the fixes

Codex (same model and settings) reviewed `fb96152e..671d4ceb`: 93 tests passed,
and it requested changes with 1 blocker, 13 major and 3 minor findings, plus
three goalpost items. Of the 18 round-1 findings it rated 8 fixed and 10 partly
fixed; every "partly" gap is one of the findings below, except two test-coverage
gaps (rows 18 and 19). Each finding, its fix, and the test that fails without
the fix:

| # | Severity | Finding | Fix | Test |
| --- | --- | --- | --- | --- |
| 1 | blocker | `run watch --interval 30 --interval -1` accepted at 30 s; gh honours `-1` and never sleeps | every `--interval` value must be a plain positive integer, else exit 64 | `classify::non_positive_or_unreadable_intervals_are_refused` |
| 2 | major | grouped shorthands (`pr list -dL1000`) hid the limit | pflag-compatible short-group parsing | `classify::grouped_shorthands_are_read` |
| 3 | major | boolean flags judged by presence (`--web=false`) | effective boolean values | `guard::uninspectable_reads_boolean_values` |
| 4 | major | `release create --notes-from-tag` passed uninspected | refused (exit 65) | `guard::notes_from_tag_is_refused` |
| 5 | major | flag-shaped alias arguments escaped inspection | every alias argument is body text | `guard::alias_flag_shaped_arguments_are_inspected` |
| 6 | major | an unknown `api` flag skipped pagination charging | pagination charged on every path | `classify::api_parser_follows_pflag_rules` and the paginate tests |
| 7 | major | wrapped lower-case base64 exempt | exemption removed (departure 19) | `guard::base64_detection` |
| 8 | major | lease descent forged by an independent open | `fdinfo` proof of the locked open file | `state::only_the_locked_open_file_proves_descent`, `cli::reopening_the_lease_file_does_not_prove_descent` |
| 9 | major | finish freed a slot a descendant still held | holder and file kept while locked | `cli::background_helper_keeps_the_write_slot` |
| 10 | major | recovery reopened early (crash mid-quarantine; limits raised later) | hard-link quarantine plus atomic save; `blocked_until`; cooldown file | `state::repeated_quarantine_keeps_every_damaged_file`, `state::cooldown_record_survives_a_damaged_state_file`, `state::damaged_cooldown_record_recovers_conservatively`, `budget::saturated_bucket_blocks_for_an_hour`, `replay::recovery_block_survives_raised_limits` |
| 11 | major | no READ room skipped overdue feedback for WRITE and SEARCH | wait for READ room | `replay::exhausted_read_budget_does_not_skip_overdue_feedback` |
| 12 | major | `run watch` makes at least 3 requests per poll, not 2 | 4 per poll plus 2 startup; `pr checks` 2 per poll | `classify::watch_loops_are_charged_and_fast_ones_refused` |
| 13 | major | stdout drained for only 2 s; scanning after forwarding | scan before queueing; complete delivery; late-signal abandon | `cli::slow_consumer_receives_every_byte` |
| 14 | major | unknown `api` flag values in the audit log | values redacted | `guard::unknown_flag_values_are_redacted` |
| 15 | minor | body lines that look like headers reopened header parsing | CRLF header-block rule | `pushback::jq_body_that_looks_like_headers_is_not_a_header` |
| 16 | minor | zombies counted as alive | `Z`/`X` are dead | `state::zombies_are_not_alive` |
| 17 | minor | sweep skipped file-free calls and judged by age alone | sweep on every paced call; lock, creator and age rules | `snapshot::sweep_removes_only_abandoned_snapshot_directories`, `snapshot::inherited_lock_outlives_the_wrapper_copy`, `cli::every_paced_call_sweeps_abandoned_snapshots`, `cli::background_helper_keeps_the_snapshot` |
| 18 | round-1 #8 gap | the watch test could not show the KILL escalation | fake gh that ignores TERM | `cli::watch_that_ignores_term_is_killed` |
| 19 | round-1 #13 gap | the refresh test checked totals, not the order | the fake gh records the saved READ window when the refresh is sent | `replay::refresh_requests_are_charged_before_they_are_sent` |

Goalpost items: the attached-shorthand guard case has full `BodySource`
equality again; the "no READ room, skip the refresh" exemption is gone (row 11);
and the wrapped-base64 exemption is gone (row 7).

Mutation checks for round 2: re-introducing the old behaviour makes the named
test fail for the `fdinfo` proof (row 8), the descendant-held slot (row 9), the
limits-independent block (row 10), the refresh wait (row 11), the 2 s drain
cutoff (row 13), three sweep mutants (no sweep in `run`, gh not inheriting the
snapshot lock, the sweep ignoring the lock; row 17), and the KILL escalation
(row 18: without it the test waited the fake gh's full 30 s).

After these fixes `cargo test -p gh-paced` runs 113 tests, all passing: 75
library, 24 CLI and 14 replay tests, against 93 at the round-2 head. The run
takes about 12.5 s of wall time, most of it the CLI tests' real child
processes (9.2 s).

## Review round 3 and the follow-up commit

Codex (same model and settings) reviewed `fb96152e..670eed40` and ran the
tests: 113 passed (75 library, 24 CLI, 14 replay). It requested changes with 1
blocker, 10 major and 2 minor findings. Of round 2's 17 findings it rated 10
fixed and 7 partly fixed, and all three goalpost items fixed. Of the 10 round-1
findings still open after round 2 it rated 4 fixed and 6 partly fixed.

Its goalpost assessment found no weakened assertion, no failure relabelled as a
pass and no deleted check. It answered "yes" on tolerances and exemptions for
four things: the watch documentation admitting requests beyond the charge, the
complete-header-block rule dropping interrupted signals, the 20-character
minimum line length for wrapped base64 (which has an accepting test), and the
post-exit output cutoffs. All four are in the known gaps below.

That was the last of the three review rounds. The commit after `670eed40` fixes
six findings and has **not** been reviewed by Codex:

| Severity | Finding | Fix | Test that fails with the old behaviour restored |
| --- | --- | --- | --- |
| blocker | `extension exec foo --help` was LOCAL, so it ran unpaced, unchecked and unaudited; gh passes `--help` on to the extension, which may ignore it | `--help` is free only on a known command path that does not run an extension | `classify::help_detection_is_not_fooled_by_a_flag_value` |
| major | `pr list --label -dL --limit 1000` cost 1: the value `-dL` was read as a group that swallowed the real `--limit` | recording a value no longer skips the token after it, so every token is still examined as a flag | `classify::grouped_shorthands_are_read` (two mutants) |
| major | `pr list --limit=0x3e8` cost 1, but pflag parses Go base-0 integers (1000) | limits are parsed with Go's integer syntax: sign, `0x`/`0o`/`0b`, leading-zero octal, underscores | `classify::limit_values_are_read_as_gh_reads_them` (new) |
| major | an alias's repeated flag was pruned to its last occurrence (`my-alias --body=<10 KiB> --body=small`) | no last-wins pruning for an alias, extension or unknown command | `guard::alias_flag_shaped_arguments_are_inspected` |
| major | an unknown `gh api` flag's value became the audit `command` (`api GET CANARY` for `api --token CANARY repos/o/r`) | such a call is recorded as `api <unparsed>` with method and endpoint `<unparsed>`; refusal reasons keep only the flag name | `classify::paginate_and_limit_costs` (two mutants) |
| minor | `pr checks 1 --watch=false` was charged 20 and refused as a watch | the effective boolean value is used; the last occurrence wins | `classify::watch_loops_are_charged_and_fast_ones_refused` |

Mutation checks: each fix was reverted on its own (8 mutants; two rows have
two) and its named test failed every time.

The same commit fixes a test flake. `snapshot::sweep_removes_only_abandoned_snapshot_directories`
failed in 1 of 8 parallel runs of the library tests on a host at a load average
of about 190, and never when run alone (0 of 25) or single-threaded (0 of 6).
Tests are threads of one process. A child that another test spawns
(`state::lease_lifetime_follows_the_open_file` spawns `sleep 2`,
`state::zombies_are_not_alive` spawns `true`) holds a copy of every descriptor,
close-on-exec ones included, until its exec, so a lock that the sweep test has
just released can still read as held. The fix is a test-only mutex,
`state::child_guard`, held by the two spawning tests until their child is
reaped and by the three tests that observe a release: the sweep test,
`snapshot::inherited_lock_outlives_the_wrapper_copy` and
`snapshot::copies_replace_the_paths_and_vanish_on_drop`. No assertion changed.
With the fix: 0 failures in 30 parallel runs. Production needs no change: a
directory kept because of such a transient holder is removed by a later sweep.

Known gaps from round 3, not fixed, all listed in the user guide's
Limitations. Line numbers refer to `670eed40`.

- Watch per-poll charges are estimates with no enforced request bound
  (major, `classify.rs:709`).
- A detected pushback is recorded only after gh's output has been delivered,
  so a consumer that stops reading delays the cooldown for other calls (major,
  `runner.rs:670`).
- A late signal waits for a blocked write: the writer thread holds Rust's
  stderr lock inside `write_all`, and the warning needs the same lock. Codex
  reproduced this; KILL still works (major, `runner.rs:452`).
- After gh exits, reading stops after 2 s of silence, 5 s, or 64 MiB, so a
  descendant that writes later loses that output and any pushback text in it
  (major, `runner.rs:394`).
- Base64 wrapped at fewer than 20 columns is not detected: `"aaaa"` repeated
  300 times and wrapped at 16 columns passes (major, `guard.rs:932`).
- A genuine header block cut off before its blank line loses its
  `Retry-After`; gh's stderr text usually still gives the 900 s floor (major,
  `pushback.rs:193`).
- A response body containing a CRLF header block can start an unneeded
  cooldown, which errs on the safe side (minor, `pushback.rs:199`).
- No test injects a crash between quarantine and the recovery save (round 2,
  row 10); that path was checked by tracing the operations only.

## Round-3 known gaps: what changed before round 4

The commit for round 4 fixes four of the six documented-gap majors and the
CRLF-body minor, and keeps two majors as documented gaps. Each fix has a test
that fails on the code before the fix (`6a37760b`, run from an export with the
new test copied in) and passes after it.

| Round-3 gap | Now | Test that fails before the fix |
| --- | --- | --- |
| Watch charges are estimates | kept: gh-paced sees the command line and output, not the requests gh makes per poll; only GitHub can count them. The guide says so | none |
| Pushback recorded only after delivery | fixed: the reader calls a hook as soon as the scanner's signals change, before queueing the chunk; the hook extends the `<account>.cooldown` sidecar under the state lock, and every load takes the later of the sidecar and the state file | `cli::pushback_is_recorded_before_output_is_delivered` (stderr text with 1 MiB unread after it; `--include` header with 1 MiB body unread) |
| Late signal waits for a blocked write | fixed: the tee writers use `write(2)` on descriptors 1 and 2 directly, without the standard library's stream locks, and after a late signal gh-paced's own messages get a 2 s deadline. A terminal's INT after gh exits now also ends gh-paced (it used to be ignored, being terminal-generated) | `cli::late_signal_ends_gh_paced_while_the_consumer_is_stalled`, `cli::terminal_interrupt_after_gh_exits_ends_gh_paced` |
| Post-exit cutoffs lose a descendant's later output | kept: gh-paced cannot know whether a process gh started will write again, and waiting for end of file would hold gh's exit status until, for example, a `--web` browser closes. Relaying it would need a drainer process that outlives gh-paced, which is not a small change | none |
| Base64 wrapped under 20 columns | fixed: consecutive base64-alphabet lines of one width (the last may be shorter) form a block at any width | `guard::base64_detection` (16- and 8-column wrapping) |
| Cut-off header block loses `Retry-After` | fixed: without `--paginate` the first block counts however it ends; with `--paginate` a block holding at least one CRLF header line counts at the end of the output | `pushback` unit tests (`single_response_reads_only_the_first_block`, the cut-short case) |
| CRLF body starts an unneeded cooldown (minor) | fixed without `--paginate` (only the first block is read); still possible with `--paginate`, where page boundaries look the same | `cli::include_body_with_crlf_headers_starts_no_cooldown` |

Correction after round 4: three of the "fixed" labels above were overstated.
The cut-off header fix still dropped a last status or header line that the
stream ended without a line ending; the late-signal fix still let the final
messages block when gh itself had died by a signal; and the under-20-column
base64 fix covered one fixed width only, so alternating 16- and 12-column
wrapping still hid an encoding. Round 4 found all three; each is fixed below.

Remaining caveats, in the guide: a signal sent while gh runs goes to gh, so if
gh exits with the consumer stalled gh-paced waits for a second signal; a signal
in the milliseconds between gh's exit and gh-paced noticing it has no effect;
the equal-width rule also refuses long lists of equal-width tokens (143 or
more 7-character short SHAs, or 251 or more 4-digit numbers, one per line).

After the follow-up commit (`6a37760b`) `cargo test -p gh-paced` ran 114
tests, all passing: 76 library, 24 CLI and 14 replay, in 12.6 s of wall time,
9.2 s of it the CLI tests. After the commit for round 4 it runs 119 tests, all
passing: 77 library, 28 CLI and 14 replay, in 12.5 s of wall time. The five new
tests are `pushback::single_response_reads_only_the_first_block` and the four
`cli` tests named in the table above.

## Round 4 and what changed before landing

Round 4 reviewed `b7f65483` (rebased onto agent-utils main `abc75c70`) and
returned CHANGES REQUESTED: 1 blocker, 13 majors and 1 minor. Its goalpost
assessment found no weakened assertion; it rejected the requirement-level
exemptions the earlier commit had documented instead of fixing (burst debt,
watch estimates, descendant cutoffs, varying-width base64). The landing commit
fixes every finding except one half of a major, which stays a documented gap.
(Round 5 disagreed: it judged findings 5, 6, 11, 12 and 13 only partly fixed.
See round 5.)
Each fix has a test that fails on the code before the fix (an export of
`b7f65483`, or the tree just before that fix, with the new test copied in) and
passes after it.

| Round-4 finding | Now | Test that fails before the fix |
| --- | --- | --- |
| blocker: an alias beneath a group (`issue publish --help`) was LOCAL, so its body escaped | an unknown word beneath one of gh's groups, `extension exec` and `copilot` are opaque WRITE, `--help` included, with every argument inspected and redacted | `classify::compound_aliases_and_passthrough_commands_are_opaque`, `guard::opaque_commands_beneath_known_families_are_inspected_and_redacted` |
| major: a cost above the burst was admitted on a full bucket | refused before gh runs (exit 75) unless it is a watch; the halved burst applies when the account-wide budget is low | `cli::a_call_costing_more_than_the_burst_is_refused_before_gh_runs` |
| major: a `--` taken as a flag value hid later cost flags; `--label -L50001` charged 501 | half fixed: classification reads past every `--`. Kept: a value shaped like `-L<n>` is still read as a limit and refused, since gh-paced does not model which flags take values; the guide gives the `--label=-L50001` workaround | `classify::a_double_dash_flag_value_does_not_hide_later_flags` |
| major: opaque commands beneath known families were scanned with the family's flag table | they use the generic table (every argument is body text, no last-wins) and the audit keeps flag names only | `guard::opaque_commands_beneath_known_families_are_inspected_and_redacted` |
| major: an alias expansion's `--input -` body was never seen | gh's `config.yml` aliases are expanded as gh does and the expansion's body sources are checked, stdin included | `cli::an_alias_expansion_has_its_body_inspected`, `alias` unit tests |
| major: base64 wrapped at varying narrow widths was missed | a block of lines of 4 or more characters that each mix upper and lower case counts together | `guard::varying_narrow_wrapping_is_detected` |
| major: a last status or header line without a line ending was dropped | single-response mode reads an unterminated last line | `pushback::single_response_reads_an_unterminated_last_line` |
| major: a late signal still blocked when gh had died by a signal | every message after a late signal has the 2 s stderr deadline, however gh ended | `cli::late_signal_after_gh_died_by_a_signal_still_bounds_the_pushback_banner` |
| major: a stalled stderr consumer held the account lock | messages decided under the lock are queued and printed after it is released; the lock wait is bounded by `GH_PACED_LOCK_WAIT` (default 30 s, then exit 70) | `cli::a_stalled_stderr_consumer_never_holds_the_account_lock`, `cli::a_lock_held_too_long_fails_instead_of_hanging` |
| major: a consumer that went away was hidden from gh | the copy stops and gh's end is closed, so gh's next write gets EPIPE/SIGPIPE as without gh-paced | `cli::a_consumer_that_goes_away_is_passed_on_to_gh` |
| major: interactive `issue create` and `pr merge` were refused | allowed; gh's editor is pointed at `gh-paced --edit-guard`, which runs the user's editor and checks the saved text | `cli::interactive_issue_create_and_pr_merge_run_on_a_terminal`, `cli::text_written_in_the_editor_is_checked` |
| major: watch requests were unbounded by their charge | watches are refused (exit 64) unless `GH_PACED_ALLOW_WATCH=1`; the refusal points at polling with plain paced calls | `cli::a_watch_without_the_opt_in_is_refused_before_gh_runs`, `classify::watches_need_an_explicit_opt_in` |
| major: descendant cutoffs discarded later output and pushback | a stream still open after the cutoffs is handed to a `gh-paced --drain` process that copies it to EOF and scans stderr for pushback, recording the cooldown | `cli::late_output_after_a_quiet_spell_is_delivered_and_scanned`, `cli::late_output_past_the_elapsed_cutoff_is_delivered` |
| major: `--paginate=false` and `--include=false` were read as true | API booleans follow pflag's value parsing and last-wins | `classify::api_booleans_honour_explicit_false_values` |
| minor: help said gh-paced never reads or prints a token | help rewritten to say that output, including credentials gh prints, passes through, and what the audit holds | none (text only) |

Found while fixing these, not by the review: the burst refusal made `gh status`
(SEARCH, cost 3, burst 2) exit 75 on every call. It is now READ with cost 10,
the READ burst (`cli::gh_status_runs_at_the_default_budgets`, which exits 75
before the fix).

Gaps kept, each stated in the guide's Limitations: the `-L<n>` flag-value case
above; extensions that call the API through a library; the base64 shapes'
blind spots; aliases charged one WRITE token whatever they expand to, with no
extra charge for a `--paginate` inside (fixed in round 5); prompt-typed titles and template
defaults the editor guard never sees; the write slot held while the user edits;
watch estimates under the opt-in; `gh status` request counts growing with
notifications; and the drainer's limits (a phrase split across the handoff, no
stdout scan after it, no audit record, interleaving with gh-paced's final
messages, and lost output with a warning when the drainer cannot start).

## Round 5 and what changed

Round 5 reviewed `0db8f2a6` and returned CHANGES REQUESTED: 1 blocker and 18
majors. Three of them (the blocker and two majors) came from one cause:
gh-paced classified and guarded the command line the caller typed, while gh
ran whatever its alias expanded to. They are fixed by changing the design, not
by patching each case.

**gh-paced now resolves gh's aliases itself and runs gh with the expansion.**
It reads gh's `config.yml` (`GH_CONFIG_DIR`, else `$XDG_CONFIG_HOME/gh`, else
`~/.config/gh`), finds the command word the way cobra does (skipping flags and
the values they consume), applies gh 2.97's rules for which names can be
aliases (not a built-in command, extensions win over aliases, `co` defaults to
`pr checkout` when no alias is set), and expands nested aliases up to 5 deep.
The fully expanded argv is what is classified, snapshotted, guarded, audited
and passed to gh (gh still reads its configuration again when it runs; round 6
declares a change in between out of scope, see below). A `!` shell alias is passed to gh as typed and stays opaque WRITE, with
every argument inspected, because gh runs it with `sh -c`.

Doubt refuses. The YAML reader is a subset of libyaml: any construct it does
not handle (tags, anchors, a key spanning lines, a file over 1 MiB, invalid
UTF-8) makes the whole file unreadable, and then any command whose first word
could be an alias is refused with exit 78; commands whose first word is one of
gh's built-in commands still run. (Round 7 showed that built-in words can be
aliases too in a file gh-paced cannot read; every command is now refused then,
see round 7.) A loop, nesting deeper than 5, or a command
word gh-paced cannot place with certainty is refused (exit 64). A wrong guess
in the other direction (reading a name as an alias that gh would not) changes
only what runs, and that is what was classified.

| Round-5 finding | Now | Test that fails before the fix |
| --- | --- | --- |
| blocker: an alias to a watch (`w: run watch 99`) bypassed the watch opt-in | the expansion is classified, so it is refused (exit 64) without `GH_PACED_ALLOW_WATCH=1`, and with it gh receives `run watch 99 ...` | `cli::an_alias_to_a_watch_loop_is_refused_like_the_watch` |
| major: valid YAML (flow mappings) and aliases after flags (`issue -R o/r upload`) hid write bodies | flow mappings, block scalars and quoted folding are read; the command word is found past flags; unreadable YAML refuses possible aliases | `cli::flow_mapping_aliases_and_aliases_after_flags_are_found`, `cli::an_unreadable_gh_configuration_refuses_only_possible_aliases` (renamed in round 7, see there), `alias` unit tests |
| major: an alias's body file could change after the check | gh receives the expansion with the body file replaced by its snapshot, like a typed command | `cli::an_alias_expansion_has_its_body_inspected` (rewritten, below) |

The other 16 majors are not fixed in this commit and stay open: pagination
whose real request count exceeds its charge; the editor resetting the size
allowance; single-case base64 such as 1,200 `A`s wrapped at varying widths; a
lock timeout dropping an observed cooldown; drainers inheriting descriptors;
the drainer losing a pushback phrase split across the handoff; unbounded pipe
readers in the rate-limit refresh; a failed refresh keeping a stale healthy
snapshot; wall-clock steps refilling budgets; `GH_PACED_MAX_WAIT` counting
requested sleeps rather than elapsed waiting; a 1e308 duration in the config
panicking; `--label -L50001` charged as a limit; `--json --json=false`
leaving a stdin source; `--editor` forms refused; a refused edit copied
without a size bound; and terminal window size not passed on.

Test changes: `cli::an_alias_expansion_has_its_body_inspected` was rewritten.
It used to assert that gh-paced checked the expansion's body while gh received
the alias name. It now asserts what gh receives: the expanded argv
(`api -X POST repos/o/r/issues/7/comments --input -`), and for a `--body-file`
alias, a snapshot path instead of the original file. Its refusals are kept
(oversized stdin and a 15,000-byte file both exit 65, the second with
`content guard: write body is 15000 bytes`). The guard's separate
alias-expansion path (`expansion_sources`) is removed, since the expansion is
now the command line. The five round-4 `alias` unit tests are kept, with
two expectations changed to follow gh: an empty `config.yml` now yields gh's
default alias `co: pr checkout` (go-gh's fallback when no alias is set)
instead of none, and an alias loop is now refused instead of expanding until
the depth limit. The other changes in them are the new return types. No other
existing assertion changed.

gh reads `config.yml` and its extensions directory again when it runs. Round 6
showed that this matters for expanded aliases too, and the coordinator
declared it out of scope; see round 6.

## Round 6 and what changed

Round 6 reviewed `24f7842b` and returned CHANGES REQUESTED: 4 blockers, 3
majors and 1 minor. Three blockers and all three majors were places where
gh-paced read gh's aliases differently from gh. The fix follows the round-5
rule: where gh-paced cannot be sure what gh reads, it refuses.

| Round-6 finding | Now | Test (the CLI tests fail on `24f7842b`'s `alias.rs`) |
| --- | --- | --- |
| blocker: a `config.yml` with NEL (U+0085) line breaks or a second byte order mark was read as one line by gh-paced and as several by gh, so gh could run an alias gh-paced never saw | gh-paced refuses to read any configuration holding NEL, LS (U+2028), PS (U+2029), a byte order mark after the first character, a control character, U+FFFE/U+FFFF, or a `---`/`...` document marker after the first setting: possible aliases exit 78 | `cli::a_configuration_with_line_breaks_gh_reads_differently_is_refused` |
| blocker: a quoted alias name with a space in its last word (`"issue 'publish hidden'"`) runs in gh as `gh issue publish`, because cobra names a command by its `Use` up to the first space | the name is placed under its first word, as cobra does; if gh also has a command of that word (a built-in or an extension), which one cobra finds first is not known, so it is refused (exit 64); a name with an empty first word is ignored, as gh cannot invoke it | `cli::an_alias_name_with_a_space_runs_as_its_first_word`, `alias::an_alias_word_with_a_space_runs_as_its_first_word` |
| blocker: a `GH_CONFIG_DIR` that is not UTF-8 was treated as unset, so gh-paced read `~/.config/gh` while gh read the other directory | a non-UTF-8 `GH_CONFIG_DIR`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME` or `HOME` makes the configuration unreadable: possible aliases exit 78 | `cli::a_gh_config_dir_that_is_not_utf8_refuses_possible_aliases` (renamed in round 7, see there), `alias::a_lookup_variable_that_is_not_utf8_makes_the_configuration_unreadable` |
| blocker: gh reads `config.yml` again after admission, so a file rewritten while the call waits can make gh run something else, expanded aliases included | out of scope, by coordinator decision (below); no code change | none |
| major: plain YAML scalars starting with `-`, `?` or `:` followed by a non-blank (`-x: issue list`) were unreadable, so valid gh configurations refused | read as libyaml reads them in block context | `cli::aliases_starting_with_dash_question_or_colon_are_read`, `alias::reads_plain_scalars_starting_with_dash_question_or_colon` |
| major: an alias named like an installed extension was refused, while gh runs the extension | the extension runs (not treated as an alias) when gh-paced is sure gh registers its extensions; gh registers none when one entry of the extensions directory fails to load, so this needs every `gh-*` entry to be a symbolic link or a directory without `manifest.yml`; otherwise the alias is still refused (exit 64) | `cli::an_extension_runs_instead_of_an_alias_of_its_name`, `alias::an_extension_runs_instead_of_an_alias_of_its_name` |
| major: an alias gh would reject for its arguments was refused with a reason quoting the expansion, which can hold a body, into stderr and the audit log | the reason names the alias only | `cli::a_refused_alias_keeps_its_expansion_out_of_the_records`, `alias::a_refusal_does_not_repeat_the_expansion` |
| minor: `classify --help` said every refusal a real run makes is shown; the user guide still said ordinary aliases get opaque redaction | the help names the refusals classify computes and those only a real run makes (content 65, budget and wait 75, unreadable configuration 78); the guide says an expanded alias is recorded with its expansion's redactions | none (text) |

**Out of scope: gh reading its configuration again.** gh-paced reads gh's
`config.yml` and extensions directory once, before admission. gh reads both
again when it runs. Anything that changes them in between, such as a process
rewriting `~/.config/gh` while a call waits for a slot, can make gh run a
command other than the one gh-paced classified and guarded: a different
expansion of a name gh-paced expanded, an alias that shadows a command
gh-paced passed through, or a changed shell alias. gh-paced paces well-meaning
agents; a process rewriting gh's configuration under a waiting call is not in
its threat model, and gh-paced makes no guarantee about it.

The same commit addresses five round-5 majors (finding numbers from the
round-5 report). Round 7 found that three of them are fixed only in part:
F5 (a title gh prompts for is still outside the shared allowance), F6
(lower-case standard base64 still escapes the varying-width rule) and F14
(the `Retry-After` cap shortened a valid server wait; round 7 below replaces
it). F16 and F18 are fixed.

| Round-5 finding | Now | Test |
| --- | --- | --- |
| F5 (partial): the editor reset the size allowance, so a call could send 8 KiB on its command line and 8 KiB more from the editor | the wrapper passes the bytes the arguments used (`GH_PACED_BODY_BYTES_USED`) to the editor shim, and all files saved in one editor session are checked together against one `max_body_bytes` | `guard::editor_text_shares_the_allowance_with_the_arguments`, `cli::text_written_in_the_editor_is_checked` (extended) |
| F6 (partial): single-case base64 (1,200 `A`s) wrapped at varying widths passed | a line counts toward the varying-width block when it holds an upper-case letter or repeats one character | `guard::varying_narrow_wrapping_is_detected` (extended) |
| F14 (partial): a duration of 1e308 in the config panicked | `cooldown_secs`, `plain_403_cooldown_secs`, `max_wait_secs`, `min_watch_interval_secs` and `GH_PACED_MAX_WAIT` must be at most 604,800 s (7 days), the rate-limit refresh timeout at most 3,600 s, and a `Retry-After` is capped at 86,400 s (that cap is replaced in round 7) | `config::durations_must_be_representable`, `pushback::a_huge_retry_after_is_capped_not_infinite` |
| F16: `workflow run --json --json=false` kept a stdin source | the last `--json` occurrence decides | `guard::a_later_false_json_flag_drops_the_stdin_source` |
| F18: the copy kept of a refused edit had no size bound | at most 1 MiB is kept, and the refusal says the rest was not copied | `editor::refused_text_is_kept_up_to_a_bound` |

Test changes in this commit:

- `cli::text_written_in_the_editor_is_checked`: the expected refusal for a
  20,000-byte edit is now `write body is 20005 bytes across 1 source(s)
  (including 5 bytes given on the command line)`, because the 5-byte title now
  counts (F5). The test also gains the shared-allowance cases.
- `alias::uncertain_names_are_refused`: the extension case is built with
  `certain: false`, the case where gh may register no extension, so its
  refusal is still asserted; a `certain: true` extension now runs instead.
- `alias::loads_aliases_and_extensions_from_disk`: `myext`, a directory
  extension without `manifest.yml`, is now not an alias (gh runs the
  extension) instead of refused. The test adds the cases that keep it refused:
  a binary extension's `manifest.yml`, and a regular file, among the entries.
- `tests/replay.rs`: the two hand-built wrapper options gain
  `body_bytes_used: 0`; no assertion changed.

No other existing assertion changed.

Still open from round 5 (11 majors, finding numbers from the round-5 report):
F2 pagination whose real request count exceeds its charge; F7 a lock timeout
dropping an observed cooldown; F8 drainers inheriting descriptors; F9 the
drainer losing a pushback phrase split across the handoff; F10 unbounded pipe
readers in the rate-limit refresh; F11 a failed refresh keeping a stale healthy
snapshot; F12 wall-clock steps refilling budgets; F13 `GH_PACED_MAX_WAIT`
counting requested sleeps rather than elapsed waiting; F15 `--label -L50001`
charged as a limit; F17 `--editor` forms refused; F19 terminal window size not
passed on.

## Round 7 and what changed

Round 7 reviewed `0042a521` and returned CHANGES REQUESTED: 2 blockers and 5
majors. The coordinator approved a narrow round 8 scope: the two blockers, the
`Retry-After` major, and wording. Both blockers let a command run without
pacing; the `Retry-After` cap let calls through before the server's wait ended.

| Round-7 finding | Now | Test (fails on `0042a521`) |
| --- | --- | --- |
| blocker: with an unreadable `config.yml`, `gh help` ran free, but gh adds its `help` command after the aliases, so an alias named `help` (`help: !!str 'run watch 99 --interval 1'`) ran a watch loop with no admission | when gh's configuration cannot be read, every command word is refused (exit 78), gh's own commands, `help`, `__complete` and extensions included; only a line naming no command (`gh`, `gh --version`, `gh --help`) runs | `cli::a_help_alias_in_an_unreadable_configuration_is_refused`, `alias::an_unreadable_configuration_refuses_every_command` |
| blocker: when the configuration could not be read, the "gh registers its extensions" certainty was lost and an extension name ran free, but gh registers no extensions when one entry (such as a stray regular file) fails, and then an alias of that name runs | same rule: no command word runs when the configuration cannot be read, so the extensions directory is not consulted then | `cli::an_extension_name_in_an_unreadable_configuration_is_refused` |
| major: the 86,400 s `Retry-After` cap shortened a valid two-day wait | a `Retry-After` is honoured in full up to 31,536,000 s (365 days); a longer or unrepresentable value starts a cooldown with no end time (`until` is the largest finite double, `f64::MAX`), so every paced call is refused (exit 75) and the message, the banner and `status` say a person must remove `<account>.cooldown` and the `cooldown` entry of `<account>.json` | `pushback::a_long_retry_after_is_never_shortened`, `cli::a_long_retry_after_is_never_shortened` |
| minor: four statements that the command checked is the command gh runs, the `classify --help` list for exit 78, and the user guide's unreadable-configuration bullet | each now says gh reads `config.yml` again and a rewrite during the call is not guarded against; `classify --help` lists exit 78 for every command under an unreadable configuration; the guide states the refuse-every-command rule | none (text) |

**Why every command, not a list of risky words.** gh-paced cannot know what an
unreadable file holds. In gh 2.97, cobra adds `help`, `__complete` and
`__completeNoDesc` after the aliases, so an alias can take those names; an
alias name quoted to include a space (`'pr x'`) adds a second command named
`pr`, which can shadow the built-in; and an extension name runs as an alias
whenever gh registers no extensions. Any command word can therefore run as an
alias. **Cost:** while `config.yml` (or a lookup variable) is unreadable,
`gh auth git-credential` is refused too, so git cannot use gh as its credential
helper, and so is `gh alias delete`, which could have repaired the file. The
refusal names the file, so the fix is to correct it or the variable.

**Why no end time, not a cap.** The coordinator ruled that a server-requested
wait is never shortened, and a cap that sleeps less would shorten it. A wait
past `GH_PACED_MAX_WAIT` already refuses rather than sleeps, so a cooldown
with no end refuses every paced call. Unpaced commands (help, `--version`)
still run. 365 days is far past any wait GitHub sends, so a value above it is
treated as a malformed or hostile header that only a person should clear. The
value round-trips through the JSON state files (the CLI test reads `f64::MAX`
back from both `<account>.json` and `<account>.cooldown`), `status` shows `no end time` and a null
`remaining_secs`, and time formatting shows `a time too far off to show`
instead of a garbage date.

Test changes in this commit, each stricter than before:

- `alias::an_unreadable_configuration_refuses_only_possible_aliases` became
  `an_unreadable_configuration_refuses_every_command`. `pr view 1`, `api` and
  `myext x` were `NotAlias` and are now refused (config); `upload` and
  `issue upload` stay refused; `help`, `help run`, `__complete pr`,
  `auth git-credential get` and `-R o/r pr view 1` are added as refused; `""`,
  `--version` and `--help` are asserted to still run.
- `alias::a_lookup_variable_that_is_not_utf8_makes_the_configuration_unreadable`:
  `pr view 1` was `NotAlias` and is now refused; `help` is added as refused
  and `--version` as running.
- `alias::loads_aliases_and_extensions_from_disk` (unreadable part): `myext`
  was `NotAlias` and is now refused; `pr view 1` is added as refused.
- `cli::an_unreadable_gh_configuration_refuses_only_possible_aliases` became
  `an_unreadable_gh_configuration_refuses_every_command`: `pr view 1` exited 0
  and started gh; it now exits 78 with the same stderr check as `up`, and gh
  never starts. The case that still runs is `--version` (exit 0, one start).
  The ambiguous-flag case is unchanged.
- `cli::a_gh_config_dir_that_is_not_utf8_refuses_possible_aliases` became
  `a_gh_config_dir_that_is_not_utf8_refuses_every_command`: the passed-through
  command exited 0 with one gh start; it now exits 78 and gh never starts.
- `pushback::a_huge_retry_after_is_capped_not_infinite` became
  `a_long_retry_after_is_never_shortened`: 400 nines and `1e400` asserted the
  86,400 s cap and now assert a cooldown with no end time; 172,800 s and
  31,536,000 s are added and asserted to be kept exactly.

No other existing assertion changed.

Left open by the round-8 scope (coordinator decision): the binary-extension
over-refusal, the U+2028 refusal of gh-written configurations, and the F5 and
F6 remainders (all listed under Open items).

## Round 8 and what changed

Round 8 reviewed `62ae6a0c` and returned CHANGES REQUESTED: 1 blocker and 3
minors (all wording). The blocker is older (unchanged since `0042a521`). The
coordinator approved a narrow round 9: the blocker in its fail-closed form,
and the wording.

| Round-8 finding | Now | Test (fails on `62ae6a0c`) |
| --- | --- | --- |
| blocker: a header line of 8,192 bytes or more was dropped, so `Retry-After: ` followed by 9,000 zeros and `172800` (two days) left the 429 without its wait and recorded the 900 s floor; an over-long unrelated header before a valid `Retry-After: 172800` did the same. With `--paginate` the block was dropped and no cooldown started at all | a header line that reaches 8,192 bytes inside a 403 or 429 block starts a cooldown with no end time (`f64::MAX`), the same state as a `Retry-After` over 365 days; the banner and the refusal name `<account>.cooldown` and the `cooldown` entry of `<account>.json`. gh-paced does not try to parse the long value. A status line that long (a long HTTP/1.1 reason phrase) still opens its block. On stderr, a `Retry-After` whose digits run past the 4,096 bytes kept between reads does the same | `cli::a_retry_after_with_leading_zeros_never_shortens_a_wait`, `cli::an_overlong_header_before_retry_after_never_shortens_a_wait` (each in both modes; on `62ae6a0c` the first mode run, without `--paginate`, records 900 s, and with `--paginate` first no cooldown is recorded), `pushback::an_overlong_header_line_never_shortens_a_wait`, `pushback::an_unreadable_stderr_retry_after_never_shortens_a_wait` |
| minor: "gh never sees the alias name" (help text, quick start) is wrong for a shell alias, which is passed to gh by name | both say it holds for ordinary aliases, and that a shell alias is passed by name, charged one WRITE, and what its shell command sends is checked only if the `gh` it runs is gh-paced | none (text) |
| minor: the user guide's "one allowance for the whole call" | the guide says the editor text shares the allowance with the arguments, files and stdin, and that a prompted title, and a second editor opening in the same call, are not counted together | none (text) |
| minor: the user guide's varying-width base64 rule said "both upper- and lower-case letters" | the guide states the rule the code applies (an upper-case letter, or one repeated character, on each line) and its limit: lower-case-only text wrapped at varying widths under 20 columns is not counted | none (text) |

**Rules of the over-long-line check.** The rule follows the coordinator's
fail-closed form. Without `--paginate`, the long line in a 403 or 429 block
commits the block and ends reading, as any line of another shape does there;
the cooldown has no end time whatever came before. With `--paginate`, reading
continues, and the block counts only if the long line ends in CR LF (as every
header gh writes does). A long line ending in a bare LF is body text and drops
the block, as a short one does. In any other block (a 200, say), a long line
ends the block, as before. 503 is not included: GitHub sends `Retry-After`
with 403 and 429, the coordinator's rule names those two, and a 503 never
started a cooldown by itself. A 503 whose `Retry-After` is readable still
starts one, through the existing "any `Retry-After`" rule, so a 503 whose
`Retry-After` sits in an over-long line is a case where a server wait is not
honoured (listed under Open items). Round 9 found two more, fixed in round 10
(see "Round 9 and what changed"), and the residual cases left after them are
listed under Open items too; this paragraph's earlier wording, "the one
remaining case", was wrong.

**The stderr rule.** The stderr scan keeps the last 4,096 bytes between reads,
and a `Retry-After` value still running at the end of what is held is read
again with the next read while its name is within those bytes. Past that, the
remaining digits would arrive without the name. The scan now marks that case,
and the verdict gives a cooldown with no end time. One read that holds the
whole value is parsed in full (9,000 zeros then `172800` read as 172,800 s).

Test changes in this commit:

- `pushback::single_response_reads_only_the_first_block`: the over-long-line
  case keeps its assertion (the 429 is read and nothing after the long line
  is) and adds one: the verdict now has no end time.

No other existing assertion changed.

## Round 9 and what changed

Round 9 reviewed `2ffcbaa1` and returned CHANGES REQUESTED: 3 blockers and 2
minors. One blocker was new in `2ffcbaa1` (a false cooldown with no end time);
the other two are older paths that shorten a server wait. The coordinator
approved fixing all five.

| Round-9 finding | Now | Test (fails on `2ffcbaa1`) |
| --- | --- | --- |
| blocker (new): with `--paginate --jq`, body text `HTTP/2.0 429 ...` LF, then 9,000 `x`, then CR LF and a blank CR LF line, after a healthy page, started a cooldown with no end time: the long CR LF line counted as a header line although it had no colon | with `--paginate`, a long line counts as a header line only if its first 8,192 bytes hold a colon, as a short header line must; a long line without one drops the block. Without `--paginate` the first block is gh's own, so a colon-free long line there still gives no end time | `cli::a_colon_free_long_body_line_starts_no_cooldown`, `pushback::a_colon_free_long_line_is_not_a_paginated_header` |
| blocker (older): with `--paginate`, output that ended inside an over-long first header line of a 429 block (`Retry-After: ` and 9,000 zeros, then `172800`, no line end) had no complete CR LF header line, so the end of the output dropped the block and no cooldown started | output that ends inside a header-shaped (colon-holding) over-long line of a 403 or 429 block commits the block, so the cooldown has no end time, as for an over-long line that did end | `cli::output_cut_inside_an_overlong_header_never_shortens_a_wait` (both modes; on `2ffcbaa1` the mode without `--paginate` already passed, and the paginated one records no cooldown), `pushback::output_cut_inside_a_long_header_never_shortens_a_wait` |
| blocker (older): the rate-limit refresh keeps the first 1 MiB of gh's stdout and stderr; a `Retry-After` cut there after its first digit read as 1 s, so the cooldown was the 900 s floor | the capture reports which streams were cut (`Captured::stdout_cut`, `stderr_cut`, read one byte past 1 MiB). The refresh prints `rate-limit refresh: gh's stderr ran past the 1048576 bytes gh-paced reads and was cut off there` and puts the same note in its audit record. On a stderr cut, `Scanner::stderr_cut_off` marks a `Retry-After` whose value (blanks, then digits) runs up to the cut as unreadable, so the cooldown has no end time | `cli::a_retry_after_cut_by_the_capture_limit_never_shortens_a_wait` (on `2ffcbaa1`: a 900 s cooldown), `pushback::a_stderr_retry_after_cut_off_never_shortens_a_wait` |
| minor: the user guide's base64 limits said lower-case-only text, and "a single-case encoding (base32, hex)", wrapped at varying widths under 20 columns is not seen; lines of one repeated character (`aaaa`) and upper-case hex are seen | the three passages state the predicate: an encoding at varying widths under 20 columns is missed when its lines lack an upper-case letter, unless each such line is one repeated character; upper-case base32 or hex is counted | none (text) |
| minor: the round-8 section above called a 503 with its `Retry-After` in an over-long line "the one remaining case" | corrected in place, with a pointer here | none (text) |

**Why the colon.** The colon test is the one a short line already passes or
fails: a short CR LF line without a colon drops a paginated block. Applying
the same test to the kept 8,192 bytes of a long line makes the two agree, so
a long line counts exactly when a short line of that shape would. It is not
applied without `--paginate`, where the first block is gh's own and no body
text can reach it. (Narrowed after round 10: the colon test now applies only
before the block's first CR LF header line; see "Round 10 and what changed".)

**Why commit at the end of the output.** A paginated reader ignores an
unterminated last line because it may be body text. A long line of a 403 or
429 block that already passed the colon test, and whose end never arrived, has
nothing left that could show it to be body text, and the wait it may hold
cannot be read. Committing the block is the fail-closed choice. A short
unterminated line is still ignored, as before. (Narrowed after round 10: only
for a block that began on the first stdout line or already holds a CR LF
header line; see "Round 10 and what changed".)

**What a cut does not cover.** `stderr_cut_off` looks only at what is held:
a `Retry-After:` name whose value runs to the cut. If the cut falls before the
name, or inside it (`Retry-Af`), the value is never seen, and the cooldown is
whatever the rest of stderr gives (900 s for an `HTTP 429`). The warning still
says that the stream was cut. A cut stdout can make the snapshot unparseable, and
the refresh then warns as before, now also naming the cut. (Wording corrected
after round 10: valid snapshot JSON followed by more than 1 MiB of blanks still
parses.)

Test changes in this commit:

- `cli::assert_overlong_header_starts_a_no_end_cooldown` takes the fake gh
  variable that carries the output (`FAKE_GH_STDOUT` for the two round-9
  tests, `FAKE_GH_STDOUT_HEAD`, which has no line end, for the new one). Its
  assertions are unchanged.
- The fake gh's `api rate_limit` branch writes `FAKE_GH_RATE_STDERR_FILE` to
  stderr when it is set.

No existing assertion changed.

## Round 10 and what changed

Round 10 reviewed `8d9f492c` and returned CHANGES REQUESTED: 3 blockers and 2
minors. The blockers and the "can make" minor are new in `8d9f492c`; the
boundary-wording minor dates from `2ffcbaa1` (corrected after round 11). One
blocker drops a server wait already read
(the unsafe direction); two start a false cooldown with no end time on body
text or prose (the fail-safe direction, which still stops every paced call
until a person clears it). The coordinator approved the fixes below, including
the trade-off in the second row.

| Round-10 finding | Now | Test (fails on `8d9f492c`) |
| --- | --- | --- |
| blocker (unsafe): with `--paginate`, `HTTP/2.0 429 ...` LF, `Retry-After: 172800` CR LF, then 9,000 `X` and `: value` CR LF: the colon after the bytes kept dropped the block and its two-day wait, so no cooldown started (or 900 s from stderr). `2ffcbaa1` gave this no end time (attribution corrected after round 11) | the colon test applies only while the block has no CR LF header line yet. After one, an over-long line, with its colon late or with none, keeps the result `2ffcbaa1` gave: a cooldown with no end time. The Open item that dated the loss to round 9 now names the commit and the narrower scope | `cli::a_long_line_after_a_retry_after_never_drops_the_wait` (both modes; on `8d9f492c` the mode without `--paginate` passes and the paginated one records no cooldown), `pushback::a_long_line_after_a_header_line_never_drops_a_wait` |
| blocker (fail-safe): with `--paginate --jq .body`, a healthy page, then body text `HTTP/2.0 429 ...` LF and `X-Long: ` with 9,000 `x` and no line end: the end of the output committed it, and every later call was refused | at the end of the output, a block cut inside an over-long header line is committed only if it began on the first stdout line or already holds a CR LF header line. Nothing can come before gh's own first block; a later block with no CR LF header line may be a page body cut short | `cli::a_page_body_cut_inside_a_long_line_starts_no_cooldown` (on `8d9f492c`: a cooldown with no end time), `pushback::output_cut_inside_a_long_header_never_shortens_a_wait` |
| blocker (fail-safe): refresh stderr of 1,048,540 `y`, then LF and `Documentation example retry-after: ` reaching the 1 MiB cut: the words in prose matched, and the refresh recorded a cooldown with no end time | `Scanner::stderr_cut_off` counts a `retry-after:` only at the start of its line: what comes before it on its line, trimmed of blanks, is empty or gh's debug marker `<`. `Scanner::feed` is unchanged: it still reads such digits anywhere, as before | `cli::a_capture_cut_after_prose_starts_no_cooldown` (on `8d9f492c`: exit 75 and a cooldown with no end time), `pushback::a_cut_after_retry_after_in_prose_is_not_a_header` |
| minor: "A cut stdout makes the snapshot unparseable" overclaims | "can make", in place | none (text) |
| minor: the Open items said "8,192 bytes or more"; a line of exactly 8,192 bytes, CR included, is read whole | "more than 8,192 bytes before the line end", in place | none (text; `pushback::an_overlong_header_line_never_shortens_a_wait` holds the boundary) |

**The trade-off in the second row.** A real 429 or 403 on a later page whose
output is cut inside its first header line, an over-long one, is dropped like
the body text it cannot be told apart from. Its cooldown is the one stderr
gives: 900 s for an `HTTP 429`, or the plain-403 cooldown, not one with no end
time. That needs gh's output to stop in the middle of a header line of more
than 8,192 bytes on a page after the first. The first page, and any block that
already holds a CR LF header line, keep the no-end result.

**Why the first CR LF header line.** Before it, a status-shaped line and a long
line may be `--jq` body text, and the colon test is what tells them apart, as
it does for a short line. After it, the block is in gh's header format (status
line, then `Name: value` CR LF), and a `Retry-After` may already have been read,
so dropping the block would lose a wait. A later long line keeps the block and
gives no end time, as in `2ffcbaa1`.

**Why the start of a line.** gh puts response headers on stderr only in its
debug output (`GH_DEBUG=api`), one to a line after a `<` marker, as in
`< Retry-After: 60`. A `retry-after:`
later in a line is prose, and a cut after it says nothing about a wait.
`Scanner::feed` is not restricted: a whole `retry-after: 60` anywhere still
lengthens the cooldown, which is the fail-safe direction.

Test changes in this commit:

- `pushback::output_cut_inside_a_long_header_never_shortens_a_wait`: the
  later-page case (`after_page`: a healthy page, then a 429 or 403 block cut
  inside `Retry-After: ` and 9,000 zeros) asserted a cooldown with no end time
  and now asserts no cooldown. **This is the one weakened assertion**, and it is
  the trade-off above. A new case `after_header` (the same block with
  `X-A: b` CR LF before the cut line) takes over the no-end assertion for a
  later page, and `after_page_before` (cut inside `X-Long: `) joins the
  no-cooldown side.
- `pushback::a_colon_free_long_line_is_not_a_paginated_header`: one comment
  now says the colon rule holds before the block's first CR LF header line; its
  assertions are unchanged.
- New: `pushback::a_long_line_after_a_header_line_never_drops_a_wait`,
  `pushback::a_cut_after_retry_after_in_prose_is_not_a_header`, and the three
  CLI tests in the table.

## Round 11 and what changed

Round 11 reviewed `65829cb0` and returned CHANGES REQUESTED: the four approved
round-10 fixes were confirmed, and it found 1 blocker and 1 minor. The blocker
shortens a server wait (the unsafe direction) and is older: it is present at
`0db8f2a6`, the base of this series.

**The finding.** Without `--paginate`, gh writes `HTTP/2.0 429 ...` LF, then
`A-Pad: ` with 4,040 `x` and CR LF, then the whole `Retry-After: 172800` CR LF
in one 21-byte write. Read from the raw terminal gh-paced gives gh as stdout,
with a 64 KiB read, the first read returned 4,095 bytes ending in
`Retry-After: 17`, in 8 of 8 of the reviewer's trials. If delivery of that
chunk to the caller's terminal fails, `runner.rs` marks delivery broken and the
reader stops after the chunk it holds, then calls the end of stdout. The
reader then supplied the missing CR, read the wait as 17 s, and stored the
900 s floor in place of 172,800 s. The Open item written after round 10 said
this needed gh's output cut inside a write, because gh writes a header line in
one write that "a pipe delivers whole". That was wrong: a terminal can hand
over part of a small write in one read, so a complete write does not give a
complete read.

**The rule now.** stdout can stop at any byte, so at its end
(`Scanner::end_of_stdout`):

- a 403 or 429 block cut short (before its CR LF blank line) keeps a wait only
  from a `Retry-After` line that arrived whole, with its line end. Without one
  the cooldown has no end time (`Scanner::cut_header_block`, reported as
  `header block cut off`). A `Retry-After` value that the cut may have
  shortened is never used;
- a last line whose line end never arrived is read as cut: without
  `--paginate`, a `Retry-After` there in a block of any status gives no end
  time. A last line that ends in CR lost only its LF and is read whole, as
  before;
- a cut line is never the blank line that confirms a block. Found while
  writing the byte sweep: a coloured header name cut inside its escape
  (`ESC[1;3`) strips to nothing, and with the supplied CR it read as the blank
  line, so a 429 with its `Retry-After` still to come got 900 s;
- with `--paginate`, the first stdout line is read even without its line end:
  only gh's status line can be there. A block that began on the first stdout
  line now counts at the end of the output whatever line it was cut in (after
  round 10 that held only inside an over-long line). A later page's block
  counts only once it holds a CR LF header line, as before: cut in its status
  line or first header line, it is dropped like a `--jq` page body cut short,
  and stderr decides (900 s for an `HTTP 429`). This is the round-10 trade-off;
  it already covered short lines (see Open items), and this change leaves it
  as it was.

RFC 9110 makes `Retry-After` a singleton field, so the first whole line is the
wait; a second line cut later does not change it.

| Round-11 finding | Now | Test (fails on `65829cb0`) |
| --- | --- | --- |
| blocker (unsafe, older): a terminal read cut inside `Retry-After: 172800`, then delivery failure, stored 900 s | a cut 403 or 429 block without a whole `Retry-After` line has no end time; a cut `Retry-After` line is never trusted | `pushback::output_cut_anywhere_in_a_header_block_never_shortens_a_wait` (every byte cut, 403 and 429, plain and coloured, both modes, and the reviewer's exact 4,095-byte read; on `65829cb0` the first failure is a lone `HTTP/2.0 429` giving 900 s), `pushback::a_later_page_cut_short_never_shortens_a_known_wait`, `cli::a_consumer_gone_mid_retry_after_never_shortens_the_wait` (the reader of a pipe gone; on `65829cb0`: paused for 900 s), `cli::a_terminal_gone_mid_retry_after_never_shortens_the_wait` (stdout a terminal whose controlling side closes after `Retry-After: 17` arrives; on `65829cb0`: paused for 1,728 s, the digits held when delivery failed) |
| minor: round 10's history said `8d9f492c` gave the late-colon line no end time, and called all five round-10 findings new | `2ffcbaa1` gave it no end time; the boundary-wording minor dates from `2ffcbaa1` | none (text) |

The two CLI tests drive the cut through gh-paced itself. The fake gh writes the
response in three steps a second apart (`FAKE_GH_STDOUT_STEPS`): `...Retry-After: 17`,
then `28`, then `00` and the rest. The two interleavings differ:

- Pipe test: the test closes the pipe's reading side before it starts
  gh-paced, so delivering the first step fails and gh-paced stops reading after
  the chunk it holds, which ends `Retry-After: 17`.
- Terminal test: the test reads the terminal until `Retry-After: 17` has
  arrived and then closes the controlling side, so delivery fails on the second
  step and the cut is at `Retry-After: 1728`.

Each asserts the
banner (`the cooldown has no end time`, `before its Retry-After was read
whole`), `cooldown.until` equal to `f64::MAX` in the saved state, exit 75 on
the next call, and that the fake gh ran once.

Fail-before evidence: the production sources of `65829cb0` with the new tests
and a compile shim (the one new field `cut_header_block`, declared and never
set), offline, separate target directory: unit 117 passed and 5 failed
(the two new tests and the three changed ones below), CLI 60 passed and 2
failed (both new), replay 14 passed.

Test changes in this commit. Each changed expectation is now stricter (a
longer or unending cooldown in place of a shorter one), except where noted:

- `pushback::single_response_reads_an_unterminated_last_line`: the input
  `HTTP/2.0 429 ...` LF `Retry-After: 3600` with no line end asserted
  `retry_after == Some(3600)` and a 3,600 s cooldown. **That assertion was
  replaced**: the same input now asserts `cut_header_block` and no end time,
  and the 3,600 s assertions moved to the input that ends in CR. The status
  line alone asserted 900 s and now asserts no end time. The 403 with
  `X-Ratelimit-Remaining: 0` cut keeps its `remaining_zero` assertion and adds
  no end time. New cases: a cut `Retry-After` in a 200 block gives no end time;
  a 200 block cut elsewhere gives no cooldown. One comment now says "after the
  first".
- `pushback::output_cut_inside_a_long_header_never_shortens_a_wait`: the last
  case (`--paginate`, `HTTP/2.0 429 ...` CR LF `Retry-After: 17`, no line end)
  asserted no cooldown and now asserts `cut_header_block` and no end time.
- `pushback::single_response_reads_only_the_first_block`: a lone 429 status
  line keeps its `http_429` assertion and adds no end time.
- New: the two unit tests and two CLI tests in the table.

## After round 11: the rebase and ending a cooldown by hand

**Rebase.** Before round 12 the series was rebased onto agent-utils main
`dceb432245cf1ce0277f68497aa36a5d9a389a27`, 3 commits past `6b891cc1`. It
applied without conflicts, and the 17 files the series touches are
byte-identical before and after. The SHAs in the round sections above are the
pre-rebase commits, kept on the local branch
`coord/gh-paced-pre-rebase-3fc9a6f3` on devbig014. Titles map one to one; the
last two are `3d12eec3` (was `65829cb0`, the head round 11 reviewed) and
`acc64154` (was `3fc9a6f3`, the round-11 fixes).

**Ending a cooldown by hand.** The coordinator asked for the operator's
procedure for a cooldown with no end time: the exact command, and what it
leaves behind. The user guide's pushback section now ends with "Ending a
cooldown by hand". The command takes `flock` on `<account>.lock`, the lock every
gh-paced process takes, around a short `python3` step. That step removes
`<account>.cooldown`, then writes `<account>.json` back with
`"cooldown": null` through a temporary file and a rename (round 12 found
that this step could leave an unreadable state file; see "Round 12 and what
changed"). The section
lists what is left: the buckets, hourly windows, in-flight writes and audit log
as they were, and no cooldown record until the next pushback. It also says what
not to do: clear one copy only, delete the state file, or clear while the call
that met the pushback still runs. There is no `gh-paced` subcommand for this;
none was asked for. The three places in the guide that said "removes
`<account>.cooldown` and the `cooldown` entry" now link to the section.

New CLI test `the_guides_command_ends_a_cooldown_with_no_end_time` takes the
block from the embedded guide, replaces only its `state=` line, and runs it with
`/bin/sh` against a real no-end cooldown (a 403 with `Retry-After: 40000000`,
refused with exit 75 before). Afterwards `status` prints `cooldown: none`, the
next call runs, the state file equals the old one apart from the entry and has
mode 0600, and the audit log is byte-identical. Fail-before, by editing the
guide's block: without its `s["cooldown"] = None` line the test fails at the
null-entry assertion; without its `os.remove` it fails at the assertion that
`<account>.cooldown` is gone. The test needs `flock` (util-linux) and
`python3` on `/usr/bin:/bin`.

## Round 12 and what changed

Round 12 reviewed `3d12eec3..3472af43` (the round-11 fixes and the clearing
procedure, after the rebase) and asked for changes: two blockers and two
minors.

| Round-12 finding | Now | Test (fails before) |
| --- | --- | --- |
| blocker (unsafe, older; present at `0db8f2a6`): after a late signal gh-paced took the scanner without feeding it the end of stdout, so an open 403 or 429 block whose whole `Retry-After: 172800` line had been read was never committed, and the cooldown was nothing or the 900 s that stderr gave | each reader first scans what it can read at once and feeds the end of its stream; gh-paced waits for the readers up to `lock_wait_secs` plus 1 s, then also feeds the end of stdout to its copy of the scanner | `cli::a_late_signal_keeps_the_wait_from_a_header_block_already_read` (on the old runner: no cooldown, 8 runs of 8), `cli::a_late_signal_scans_output_gh_wrote_but_gh_paced_had_not_read` and `cli::a_late_signal_waits_for_a_reader_held_up_by_the_state_lock` (without the reading step, or without the wait: both fail, 3 runs of 3; with a fixed 1 s wait the second fails, 5 runs of 5) |
| blocker (new in `3472af43`): the guide's clearing command opened its temporary file with mode 0600 through `os.open`, so under `umask 0777` the state file became mode 000, and a stale read-only `.clearing` file stopped the command | the command writes to a fresh `mkstemp` file in the state directory and sets mode 0600 on it with `fchmod` before the rename, and removes it if a step fails | `cli::the_guides_command_ends_a_cooldown_with_no_end_time`, extended (on the old command: `PermissionError` on the stale file; with the stale file gone and the umask kept: the test cannot read the state file) |
| minor: the guide said the refresh after a cooldown happens at most once every 60 s | "at most one every `rate_limit_min_refresh_secs`, 60 s by default" | none (text) |
| minor: this note said both cut-output CLI tests close the reading side after `Retry-After: 17` arrives | the two interleavings are described separately under "Round 11 and what changed" | none (text) |

**The late-signal fix has three parts.** A late signal is an INT, TERM, HUP or
QUIT that arrives after gh has exited, while gh-paced is still delivering gh's
output. gh-paced then abandons delivery, does not join the reader threads (one
may be stuck behind a consumer, or waiting for the state file's lock to record
a cooldown), copies the scanner, records the cooldown from the copy, prints
the banner, and dies by the signal.

1. Scanning output not yet read. Since gh has exited, what is still in its pipe
   is output gh already wrote, at most one pipe's capacity (64 KiB by default
   on x86-64; gh-paced does not resize its pipes, and 1 MiB is the default
   limit on resizing by an unprivileged process). On its next poll each reader
   now reads what it can without waiting, up to `LATE_SCAN_BYTES` (1 MiB;
   renamed `READY_SCAN_BYTES` in round 13, when the cutoffs began to use it),
   scans it, delivers none of it, feeds the end of its stream to the scanner
   (which commits an open block as output cut short), sets its new `scanned`
   flag, and stops. Before, the reader stopped without reading, so a 429 page
   that gh wrote while the reader was busy was lost.
2. The wait. gh-paced waits for every reader's `scanned` flag before it copies
   the scanner, up to `lock_wait_secs` plus `LATE_SCAN_WAIT_SECS` (1 s). A
   reader can be held up only by its hook, which waits up to `lock_wait_secs`
   for the state file's lock, and by its 100 ms poll; reading is bounded by
   the 1 MiB. The new field `Invocation::hook_wait_secs` carries
   `lock_wait_secs` to the runner (0 for the rate-limit refresh, which has no
   hook). When the lock is free the readers finish within about 100 ms, so
   gh-paced still dies promptly; when it is contended, gh-paced would wait for
   the same lock to record the cooldown anyway. The first version of this fix
   waited a fixed 1 s; a reader that was still waiting for the lock after
   that was left out of gh-paced's copy, so gh-paced recorded 900 s while the
   reader's own hook recorded the two days only if it won the race against
   the process ending.
3. End of stdout on the copy. After copying the scanner, gh-paced feeds it the
   end of stdout when stdout is teed, in case the stdout reader did not finish
   within the wait (if it did, the second end of stdout changes nothing:
   `end_of_stdout` is idempotent). With the wait in place no test reaches this
   step: removing it leaves all five late-signal tests passing, 3 runs of 3.
   It is kept as the fallback for a reader that never finishes (for example
   one that panicked).

The tests check what gh-paced itself recorded, not only the state file. A
reader's hook can write the two-day cooldown to the state file just before the
process ends, so the state file alone passes by luck: a fixed 1 s wait passed
4 runs of 5 on it. The shared helper `assert_two_day_cooldown` therefore also
requires gh-paced's PUSHBACK banner to give a pause between 172,790 s and
172,900 s and not to say "an existing longer cooldown stays". The banner comes
from gh-paced's copy of the scanner; when the copy lacks the second page and
the reader recorded first, it reads "paused for 172800 s ... (an existing
longer cooldown stays)", which is what the fixed-1-s build printed. With the
fix, gh-paced reads the clock after taking the lock, so a reader's record
already in the state file always ends before gh-paced's own.

The fake gh gained `FAKE_GH_HOLD_OUT=<n>`: a helper process that keeps gh's
stdout open after gh exits and writes `X-Held: <i>` CR LF to it every 0.4 s,
`n` times. That keeps gh-paced reading stdout, with the block open, when the
test sends TERM 1 s after gh's exit. The two unread-page tests hold the
account lock from gh's start, so the stdout reader is in its hook (recording
the first page's 900 s) when gh writes the second page and exits; they let the
lock go 0.1 s and 2.5 s after TERM.

**The clearing command.** Besides `mkstemp` and `fchmod`, nothing else in
the procedure changed: `flock` on `<account>.lock`, remove
`<account>.cooldown`, write `"cooldown": null`, rename. The temporary file is
now named `.<account>.json.clearing-<random>`, exists only until the rename,
and is removed if any step fails. The test runs the guide's block under
`umask 0777`, with a read-only `test.json.clearing` (the old command's file
name) in the state directory. It asserts what it asserted before, plus: the
state file is mode 0600, the stale file is untouched, and the state directory
afterwards lists exactly what it listed before minus `test.cooldown`, so the
command leaves no file of its own.

**Fail-before evidence** (on devbig014, in `ignored/coordw-ghpaced-r5/`,
directories `r14-failbefore/` and `r15-failbefore/`; each build used the new
tests):

- `runner.rs` of `3472af43`: the first test failed 8 runs of 8 with
  `"cooldown": null` in the state file.
- The fixed runner without the reading step: both unread-page tests failed,
  3 runs of 3 (with the state-file check alone the 0.1 s test failed 5 runs of
  5 with a 900 s cooldown).
- The fixed runner with no wait at all: both unread-page tests failed, 3 runs
  of 3. (With the state-file check alone and the 0.1 s test only, this passed
  3 runs of 3; that is what made the banner check necessary.)
- The fixed runner with a fixed 1 s wait: the 2.5 s test failed 5 runs of 5
  on the banner check (on the state-file check alone it failed 1 run of 5,
  with 902 s).
- The fixed runner without the end of stdout on the copy: all five late-signal
  tests passed, 3 runs of 3 (see part 3).
- The guide's old block with the stale file present: `PermissionError` on
  `test.json.clearing`. The old block under `umask 0777` alone: the test's next
  read of the state file failed with `EACCES`.

**Test changes in this round.** Three new CLI tests and the extended clearing
test, all described above. No existing assertion was removed or loosened. The
clearing test's assertion that `test.json.clearing` is absent afterwards was
replaced by the stricter directory-listing assertion, because the test now
creates a stale file with that name on purpose and requires the command to
leave it alone. The three new tests share the helper
`assert_two_day_cooldown`, which checks the banner as above and that both
records of the cooldown (`test.json` and `test.cooldown`) end between
172,790 s and 172,900 s after the signal.

## Round 13 and what changed

Round 13 reviewed `3d12eec3..61f8ab4e` (the round-12 fixes) and asked for
changes: one blocker and three minors.

| Round-13 finding | Now | Test (fails before) |
| --- | --- | --- |
| blocker (shortens a wait; older than this series): a reader that had stopped at an after-exit cutoff left gh's own output unread, and that output was then either handed to the drainer, which does not scan stdout, or, after a late signal, dropped. A third 429 page with `Retry-After: 172800` was lost and the cooldown was the 1,200 s of the second page | before a reader stops at a cutoff it reads, scans and queues for delivery whatever can be read at once, up to `READY_SCAN_BYTES` (1 MiB) | `cli::a_reader_stopped_after_gh_exits_still_scans_what_gh_wrote` (no signal) and `cli::a_late_signal_after_a_reader_stopped_keeps_what_gh_wrote` (TERM 1 s after the lock is released): both failed on `61f8ab4e` with "the banner gives 1200 s" |
| minor: the two unread-page tests of round 12 relied on taking the lock during the fake gh's 1 s sleep, so they could pass without the reading step | the fake gh's new `FAKE_GH_GO=<path>` makes it wait (up to 30 s) for that file before writing; the tests create it only after they hold the lock | the same two tests with the late reading step removed: both fail, 1 run of 1 (900 s) |
| minor: the guide said the clearing command's temporary file is "removed if the command fails" | the guide says it is removed when a step fails with a Python error, and that a kill before the rename can leave it behind, with `<account>.json` unchanged; gh-paced ignores it, and it is safe to delete | none (text) |
| minor: the 1 MiB bound could be overshot by up to 64 KiB, because each read asked for the whole buffer | each read asks for at most what is left of the 1 MiB | none (the overshoot changed no wait) |

**Why the cutoff was reachable with gh's output unread.** The after-exit
cutoffs (2 s of silence, 5 s after gh's exit, 64 MiB) are measured from the
reader's last read. A reader that records a cooldown calls its hook, which
waits up to `lock_wait_secs` for the state file's lock. Time spent there
counts as silence, so the next loop can take the 2 s cutoff (or the 5 s one)
without having read what gh wrote while the hook waited. Before this round the
reader then fed the end of the stream to its scanner and returned the stream,
and main handed it to `gh-paced --drain`, which copies stdout unscanned. The
round-13 review traced this for the late-signal path; the same test without a
signal shows it on the ordinary path too, where it is older than this series:
the drainer has never scanned stdout.

**The fix.** `take_ready` (it replaces round 12's `scan_ready`) reads while
the stream is readable without waiting, at most `READY_SCAN_BYTES` in all,
scans each read, and, at a cutoff, queues it for delivery like any other read.
The late path calls it without delivery, as before. Since gh has exited by the
time either path runs, what is still in its pipe is output gh already wrote:
at most one pipe's capacity (64 KiB by default), or a terminal's buffer. So
the drainer is now left only with what a process gh left behind writes after
the cutoff, which is what the user guide's limits already described.

**The new tests.** Both use `--include --paginate` with stdout on a
pseudo-terminal, `lock_wait_secs` 3, and the account lock held from before gh
starts (the `FAKE_GH_GO` handshake). The fake gh writes a 200 page with a
100 KiB body, then 429 pages with `Retry-After` 60, 1200 and 172800, separated
by 5 KiB of body, and exits 1. The reader's hook for the first 429 waits 3 s;
gh exits during it; the reader reads the 1200 page, waits 3 s in its hook
again, and then takes the cutoff with the 172800 page unread. The test
releases the lock 6.5 s after gh's exit. Without a signal the terminal is
read throughout; the test asserts exit status 1, that gh-paced ran more than
6 s after gh's exit, the two-day cooldown through `assert_two_day_cooldown`,
and that the terminal received all three `Retry-After` lines in order and all
110 body lines, which catches a fix that scans but does not deliver. With the
signal the terminal is never read and TERM arrives 1 s after the lock is
released; the test asserts death by TERM, the late-signal message, and the
two-day cooldown.

**Fail-before evidence** (devbig014, `ignored/coordw-ghpaced-r5/`, logs
`r17-before-fix.log` and `r17-failbefore/`; each build used the new tests.
`r17-before-fix.log` ran 5 CLI tests selected by name, both new tests among
them; each `r17-failbefore/` build ran the 8 late-signal and stopped-reader
CLI tests):

- `runner.rs` of `61f8ab4e`: both new tests failed with "the banner gives
  1200 s".
- The fixed runner without the read at the cutoff: the same two failed, with
  "the banner gives 1200 s".
- The fixed runner without the late read: `a_late_signal_scans_output_gh_wrote_but_gh_paced_had_not_read`
  and `a_late_signal_waits_for_a_reader_held_up_by_the_state_lock` failed
  with "the banner gives 900 s"; the two new tests passed, because the read
  at the cutoff already covers them.
- The fixed runner reading at the cutoff without delivering: the scanner had
  the two days, but `a_reader_stopped_after_gh_exits_still_scans_what_gh_wrote`
  failed on the delivered `Retry-After` values, `["60", "1200"]`.
- The fixed runner: all 8 passed, 3 runs of 3 (8.6 s to 9.4 s each).

**Test changes in this round.** Two new CLI tests, the helper
`assert_stopped_reader_scans_what_gh_wrote` they share, the fake gh's
`FAKE_GH_GO`, and the handshake in the two round-12 unread-page tests, which
replaces their `FAKE_GH_SLEEP=1`. The round-12 description above ("hold the
account lock from gh's start") is now exact: gh writes nothing before the test
holds the lock. No assertion was removed or loosened.

## Round 14 and what changed

Round 14 reviewed `61f8ab4e..50ebf880` (the round-13 fixes) and asked for
changes: one high and four minors. As the coordinator directed for this
round, only the high is fixed in code; three minors are recorded under "Open
items", and the fourth, a false sentence in this note, is corrected.

| Round-14 finding | Now | Test (fails before) |
| --- | --- | --- |
| high (a hang, neither shortens nor lengthens a wait; older than this series, reused by round 13): a read after a poll that reported the stream readable could wait indefinitely | the reader reads gh's stream non-blocking (`ReadEnd`); a read that finds nothing returns at once | `runner::tests::a_read_after_a_stale_hang_up_report_does_not_wait`: failed with `Err(Timeout)` when the flag is not set |
| minor: the lock handshake does not force an unread second page | not changed; see "Open items" | none |
| minor: the "more than 6 s after gh's exit" assertion is checked after the test's own 6.5 s sleep | not changed; see "Open items" | none |
| minor: the module documentation says the hook is called before the chunk that changed the signals is queued, which the stop-time read does not do | not changed; see "Open items" | none |
| minor: the round-13 evidence said every build ran 8 tests | corrected in "Round 13 and what changed" | none (text) |

**The race.** The reader's `readable` is a `poll` for input that counts any
event, and a pseudo-terminal master reports a hang-up when the last slave
descriptor closes. A process gh left behind can close its slave descriptor,
so the master reports the hang-up, and open `/dev/pts/N` again before the
reader's read. The read then finds nothing and, blocking, waits for that
process's next write. Round 14 reproduced the sequence with system calls:
poll returned 16 (the hang-up), opening the slave again removed it, and the
read returned only after a write 0.35 s later. The reader's main loop has
polled and then read this way since the tee was written; round 13's
stop-time read did the same at the cutoff. A reader stuck there reaches no
cutoff, so gh-paced's finish and the drainer handoff stall; after a late
signal, gh-paced uses its bounded fallback while the reader stays stuck.

**The fix.** The reader holds gh's stream as a `ReadEnd`, which sets
`O_NONBLOCK` on it while the reader holds it. `ReadEnd::read_chunk` retries
`EINTR` and returns `Chunk::NotReady` for `EAGAIN`; end of file, `EIO` (a
master whose slave is closed) and other errors are `Chunk::End`, as before.
In the main loop `NotReady` goes back to the cutoff checks and the 100 ms
poll; in `take_ready` it ends the stop-time read. gh-paced creates both kinds
of stream itself (the pipe read end through `Command`, the master with
`O_CLOEXEC`), so no other process shares the open file description whose
flag changes. A stream handed to the drainer gets its flags back first
(`ReadEnd::into_blocking`): the drainer copies with blocking reads until the
last writer closes the stream, and with `O_NONBLOCK` left set its first read
in a pause would fail with `EAGAIN` and end the copy. So the stop-time read
now waits for nothing: a process that keeps writing holds the reader for at
most the time to read and scan `READY_SCAN_BYTES` (1 MiB), and a silent one
not at all.

**Tests and fail-before evidence** (devbig014,
`ignored/coordw-ghpaced-r5/r19-failbefore/`: `summary.txt`, the runners
`runner.good.rs`, `runner.noblock.rs`, `runner.noblockback.rs`, and one log
per run). These are the first unit tests in `runner.rs`.

- `a_read_after_a_stale_hang_up_report_does_not_wait` opens a
  pseudo-terminal, closes the slave, checks that the master polls readable,
  opens the slave path again with `O_NOCTTY`, checks that it no longer does,
  and reads in a thread. It expects `NotReady` within 2 s; otherwise it writes
  one byte to the slave to free the read, then fails. With `O_NONBLOCK` not
  set (`noblock`, the blocking read of `50ebf880`): failed with
  `Err(Timeout)`.
- `a_stream_handed_on_reads_blocking_again` checks that `O_NONBLOCK` is set
  while the reader holds the stream and that the flags are as before after
  `into_blocking`. With the restore removed (`noblockback`): failed, flags
  34818 (`O_NONBLOCK` still set) against 32770. The same runner also failed
  the two drainer CLI tests, `late_output_after_a_quiet_spell_is_delivered_and_scanned`
  ("line 1" never arrived) and `late_output_past_the_elapsed_cutoff_is_delivered`
  ("line 12" never arrived). `noblock` failed this test too.
- The fixed runner: the 2 unit tests and 12 CLI tests selected by name
  (late-signal, stopped-reader, late-output and background-helper tests) all
  passed, 3 runs of 3, 8.6 s to 9.6 s each.

No test was changed and no assertion was removed or loosened.

## Test changes worth a reviewer's attention

Round 1 changed these existing tests. Every change makes the test stricter or
follows a behaviour change the review asked for:

- `budget`: a cost above the hourly cap now gives an infinite wait (it was
  admitted into an empty window).
- `pushback`: the plain-403 cooldown in the test config went from 120 s to
  1,800 s, because 120 s is now below the 900 s floor and is a config error.
- `classify`: a paginated write costs 10 (was 1); `my-alias --help` is WRITE
  (was LOCAL); flag names in reasons are the canonical long forms.
- `guard`: base64 cases now include prose (longest run 5), a 1,600-character hex
  dump (counts 1,600), a 2,000-dash ruler (counts 2,000), and 4,000 repeated
  `x` (refused); before, digit-free runs were exempt.
- `state`: quarantine now expects saturated windows, a cooldown and restored
  leases (it expected empty buckets); `reap` returns the reaped holders.
- `config`: the floors are asserted; values that used to be accepted below them
  are now errors.
- `cli::stdin_reaches_gh_untouched`: a READ now proves 100,000 bytes of stdin
  reach gh unread; `api graphql --input -` is asserted to be WRITE and replayed
  byte for byte (an earlier comment called it READ, which was wrong).
- `IN_FLIGHT_POLL_SECS` went from 2 s to 5 s, with a warning on every poll
  instead of every 30 s.
- `replay::corrupt_state_pauses_the_account_for_an_hour` asserts the hourly
  budget text in the refusal, because a refusal names the longest wait (the
  3,600 s window, not the 900 s pause); the pause is asserted through the saved
  state file instead.

- `concurrent_processes_share_one_write_budget` (in `tests/cli.rs`) checks the
  write spacing on the admission times gh-paced records under the shared lock,
  not on the fake gh's own start times, which include bash start-up jitter. The
  spread of the fake gh's start times is still asserted (at least 1.8 s for three
  writes at one per second). Round 1 noted that those admission times were then
  sampled before the lock; they are now sampled after it, and
  `replay::time_is_read_after_the_lock_is_taken` checks that directly.
- The `ratelimit` unit test's post-reset expectations were corrected during
  development. The rule they check: a resource whose reset has passed counts as
  full; READ and WRITE use `core` and `graphql`; SEARCH also uses `search`.
- Bugs found and fixed during development: a floating-point refill that could
  spin on a sub-millisecond wait; the child inheriting gh-paced's blocked signal
  mask; overriding a signal the caller had set to ignored (as `nohup` does); a
  guard that reported only its first reason; misaligned `status` columns.

Round 2 changed these existing tests:

- `classify::watch_loops_are_charged_and_fast_ones_refused`: the deadlines went
  from 300 s to 120 s (`run watch`) and from 600 s to 270 s (`pr checks`), the
  new per-poll estimates.
- `cli::watch_past_its_deadline_is_stopped`: `watch_cost` in the test config
  went from 2 to 6, so the 2-token startup plus 2 polls of 2 requests still
  give the 2 s budget the test asserts.
- `guard::base64_detection`: 40 bare SHAs now count 1,600 (they counted 40).
- `guard`: the attached-shorthand case has full equality again (round 2's
  goalpost finding).
- `state::corrupt_state_is_quarantined_conservatively` and
  `budget::saturated_bucket_blocks_for_an_hour`: `hour_used() == 30` became
  `blocked_until == now + 3,600 s`, because the recovery block is no longer a
  window entry; the budget test adds caps of 1, 30, 500 and 5,000 and requires
  the block to hold under each. The quarantine test's
  `!paths.state().exists()`, which asserted the gap row 10 closed, became
  "the saved recovery state equals the loaded one".
- `replay::corrupt_state_pauses_the_account_for_an_hour`: the refusal text is
  now `read budget blocked until <time> after state recovery` (it was the
  500/hour window text), and the saved buckets are asserted to carry the block
  and no tokens. The refusal at +901 s and the run at +3,601 s are unchanged.
- `state::inherited_file_ids_include_open_files` became
  `only_the_locked_open_file_proves_descent`, and
  `snapshot::sweep_removes_only_old_snapshot_directories` became
  `sweep_removes_only_abandoned_snapshot_directories`, following the new rules.

The commit for round 4 changes three existing tests, each to a stricter
expectation that follows a fix:

- `guard::base64_detection`: 200 lines of `abcdefghij` counted 10 and now count
  2,000 (the equal-width rule refuses them).
- `pushback`: a header block cut off before its blank line gave no cooldown and
  now gives its `Retry-After` (3,600 s), also when cut inside its next header
  line.
- `cli::slow_consumer_receives_every_byte` now runs `api --include --paginate`
  (it ran `api --include`): its header block follows 1 MiB of body, which only a
  paginated call can print, and a single-response call now ignores it. Every
  byte is still asserted to arrive.

The landing commit after round 4 changes these existing tests:

- `classify::search_commands`: `status` moved from SEARCH cost 3 to READ cost
  10, and the test asserts that 10 fits the default READ burst.
- `guard::uninspectable_forms_are_refused` and
  `guard::uninspectable_reads_boolean_values`: interactive `issue create` and
  `pr merge` moved from the refused list to the allowed list, because round 4
  required them to run; the editor guard tests above cover their text.
- The watch-loop unit tests run with `GH_PACED_ALLOW_WATCH=1` in their config
  (`clsw`); their assertions are unchanged.
- `cli::watch_past_its_deadline_is_stopped` and
  `cli::watch_that_ignores_term_is_killed` set `GH_PACED_ALLOW_WATCH=1`, and
  the fake gh's `FAKE_GH_SLEEP` sleep now sends its own stdout and stderr to
  `/dev/null`. The timing ranges (1.9 to 7.5 s, 6.9 to 20 s) are unchanged.
  Why: both tests time `Command::output()`, which returns when the caller's
  stderr reaches end of file. The deadline kills the fake gh, a shell, and
  leaves its `sleep` running as an orphan that held gh's stderr. Before the
  drainer, gh-paced closed that stream at the 2 s silence cutoff. With the
  drainer it is relayed until the orphan exits, as plain gh would leave it,
  so the tests measured the sleep (8.06 s and 30.09 s) instead of the
  deadline. The real gh is one process and leaves no such orphan. A
  descendant that holds stderr after a deadline goes through the same
  drainer path the late-output tests cover; no test now combines a deadline
  with such a descendant.
- `tests/replay.rs` sets the two new `Ran` and wrapper fields (`cut_off: false`,
  `self_exe: None`); no assertion changed.

The follow-up commit after round 3 removes or loosens no assertion. It adds
assertions to five existing tests (help detection, grouped shorthands,
pagination and limit costs, watch loops, alias inspection), adds one test
(`limit_values_are_read_as_gh_reads_them`), and makes five tests take
`state::child_guard` (see round 3).

## Open items

- Budgets are per host; hosts do not share state. More than four hosts on one
  account need a tighter config file.
- Classification works from command lines, not HTTP requests.
- Pushback detection depends on gh's error wording.
- Watches need `GH_PACED_ALLOW_WATCH=1`. The per-poll figures are estimates
  from gh's code paths, not measurements. A `run watch` on a run with more than 100 jobs, or with several
  jobs that fail during the watch, can make more requests than it paid for
  before the deadline stops it; the deadline still bounds its wall time and the
  30 s floor its rate.
- Output a process gh started writes after the post-exit cutoffs goes to a
  drainer, which has the limits listed under round 4.
- A flag value shaped like `-L<n>` is read as a limit (see round 4).
- With `--paginate`, a page body containing a CRLF header block can start an
  unneeded cooldown.
- No crash-injection test for quarantine (see round 3).
- 11 round-5 majors are open (listed at the end of round 6).
- gh reads its configuration again when it runs; a change while a call waits
  is out of scope (see round 6).
- A binary extension (a `gh-<name>` directory with `manifest.yml`) makes
  gh-paced unsure that gh registers extensions, so an alias with the name of
  any installed extension is refused (exit 64) although gh runs the extension.
  gh 2.97 only stats that manifest when it registers extensions. This
  over-refuses; it does not bypass pacing.
- A `config.yml` holding U+2028 or U+2029, which gh's own YAML writer can emit
  inside a quoted alias, is unreadable to gh-paced, so every command is refused
  (exit 78) until it is rewritten.
- F5 remainder: a title gh prompts for on a terminal is not counted with the
  editor body, so the two can exceed `max_body_bytes` together, and a second
  editor opening in one call is checked against what the command line left,
  not against the first opening. The user guide says so (round 8).
- F6 remainder: lower-case standard base64 wrapped at varying widths (for
  example `abcd` repeated, at alternating 16 and 12 columns) passes the
  varying-width rule. The user guide states the rule and this limit (round 8;
  wording corrected after round 9).
- Server waits that can still go unhonoured or be shortened:
  - a 503 whose `Retry-After` is in a header line of more than 8,192 bytes
    before the line end starts no cooldown; the over-long-line rule covers 403
    and 429 only (round 8; boundary wording corrected after round 10);
  - with `--paginate`, when the first header line of a 403 or 429 block (the
    line after its status line) has a name longer than 8,192 bytes, no colon is
    in the bytes kept, so the line drops the block like body text, and a
    `Retry-After` after it is lost. This came with the colon rule in `8d9f492c`
    (the fixes after round 9, reviewed in round 10); since the fixes after
    round 10 it applies only before the block's first CR LF header line;
  - with `--paginate`, a later page's 403 or 429 block whose output ends inside
    its first header line, an over-long one, with no CR LF header line before
    it, is dropped as a `--jq` body cut short would be, so the cooldown is the
    one stderr gives (900 s for an `HTTP 429`), not one with no end time. This
    is the trade-off accepted after round 10 (see "Round 10 and what changed");
  - with `--paginate`, output that ends inside a short first header line of a
    later page's block (`HTTP/2.0 429 ...` LF, then `Retry-After: 172800` with
    no line end) is dropped, as body text would be, so the cooldown is the one
    stderr gives. The rule predates round 8; it is the short-line form of the
    trade-off above. Since the fixes after round 11, gh's own first block
    (one that began on the first stdout line) counts, with no end time;
  - with `--paginate`, a cut last line after the first stdout line is not
    read, so a `Retry-After` cut there in a block of a status other than 403
    or 429 gives nothing (a 403 or 429 block gives no end time). This predates
    this series (`0db8f2a6`); GitHub sends `Retry-After` with 403 and 429;
  - a `Retry-After` in the HTTP-date form (RFC 9110 allows it) is not parsed,
    so a 429 carrying one gets the 900 s floor. GitHub documents the
    delta-seconds form. Not changed in this series; noted after round 11;
  - the rate-limit refresh keeps 1 MiB of stderr; a `Retry-After` whose name
    falls after the cut, or is cut inside the name, is not seen, so the
    cooldown is the one the rest of stderr gives. The cut is reported (round 9).
    Since the fixes after round 10 a cut counts only for a `retry-after:` at
    the start of its line, after blanks or gh's debug `<` marker; the same words
    later in a line, cut in their value, read as the digits held;
  - output that stops before the status code arrives leaves nothing on stdout
    to read, so stderr decides, as for a call without `--include`.
- Fixed after round 11: output that ended inside the digits of a short
  `Retry-After` value (`Retry-After: 17` with no line end) read the digits
  that arrived. The item here said this needed gh's output cut inside a write,
  because a pipe delivers a small write whole; round 11 showed a terminal read
  cut inside one write, and delivery failure then ends the reading there. See
  "Round 11 and what changed".
- Cooldowns with no end time that the cut-output rule can start without a
  limit (the fail-safe direction; each stops paced calls on the host until a
  person clears the cooldown, as the user guide's "Ending a cooldown by hand"
  describes):
  - `gh api -i ... | head -n1`, or any consumer or terminal that goes away, or
    a kill, cutting a 403 or 429 block before a whole `Retry-After` line,
    including a 403 that was only a permission error, which would otherwise get
    the plain-403 cooldown;
  - with `--paginate`, a page body printed with CR LF line endings that is cut
    at the end of the output inside a block shape it holds, which got 900 s
    before;
  - a 403 cut after the place where gh's sorted header order would already
    have put `Retry-After` (gh-paced does not use that order);
  - without `--paginate`, a `Retry-After` line cut in a 200 block.
- A `Retry-After` above 365 days stops paced calls for the account on the host
  until a person clears the cooldown (see round 7).
- After a late signal (see "Round 12 and what changed"), gh-paced waits for
  its readers at most `lock_wait_secs` plus 1 s. A reader that has not
  finished by then (it would have to be stuck outside its hook, or have
  panicked) has the output it has not read left out of what gh-paced records.
  A reader that stops, whether at a late signal or at an after-exit cutoff
  (since round 13), first scans at most 1 MiB of unread output, only what can
  be read without waiting: all that gh wrote unless its pipe was resized
  beyond that, but not everything a process gh left behind may still write;
  after a cutoff, that later output goes to the drainer, which does not scan
  stdout. While the lock
  is contended, gh-paced can take up to that bound, plus its own wait for the
  lock, to die after a late signal. No test reaches the fallback that feeds
  the end of stdout to gh-paced's copy.
- A late signal commits a 403 or 429 header block still open on stdout as
  output cut short, so a block without a whole `Retry-After` line by then gives
  a cooldown with no end time. gh's own output is all scanned first (see the
  item above), so this needs a block that gh itself left unfinished, one that
  a process gh left behind is still writing, or a reader that did not finish
  within the wait. This is the fail-safe direction, like the items above.
- Round-14 minors, not changed (see "Round 14 and what changed"):
  - the tests that hold the account lock fix when the reader takes the lock,
    not how far gh has written by then. A stdout reader delayed until both
    small pages are written scans both in one read before its hook, so
    `a_late_signal_scans_output_gh_wrote_but_gh_paced_had_not_read` and
    `a_late_signal_waits_for_a_reader_held_up_by_the_state_lock` could then
    pass without the late read; and if the first hook of the two round-13
    tests starts long enough after gh's exit, the second can take the
    released lock before the 2 s cutoff, so they could pass without the read
    at the cutoff. Each mutant run so far failed as expected; an
    acknowledgement from the reader would make that independent of
    scheduling;
  - in `assert_stopped_reader_scans_what_gh_wrote`, the check that gh-paced
    ran more than 6 s after gh's exit comes after the test's own 6.5 s
    sleep, so a gh-paced that had already exited passes it. The cooldown and
    delivery checks are unaffected;
  - the `runner.rs` module documentation says the reader calls the pushback
    hook before queueing the chunk that changed the signals. The stop-time
    read at a cutoff queues what it reads first and the reader calls the hook
    once afterwards, so that output can reach the consumer before its
    cooldown is recorded.
