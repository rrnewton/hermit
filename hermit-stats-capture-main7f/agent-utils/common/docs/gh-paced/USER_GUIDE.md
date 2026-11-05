# gh-paced user guide

`gh-paced` is a client-side rate limiter for the GitHub CLI. It sits between the
caller (a person, a script, an agent, or an account wrapper script) and the real
`gh` binary. Each call is classified, charged against a budget, and delayed with
a loud stderr warning when that budget is spent. Otherwise the call passes
through unchanged.

## Why it exists

`gh` itself does no pacing. Its only related feature is the response cache,
`gh api --cache <duration>`. A loop that calls `gh` runs as fast as GitHub
answers. GitHub limits this in two ways (see [Budgets](#budgets-and-the-github-limits-behind-them)):

- **Primary limits** are counters you can read: 5,000 REST requests per hour,
  5,000 GraphQL points per hour, and 30 search requests per minute, per user.
- **Secondary limits** cannot be read in advance. They cover concurrency,
  requests per minute, CPU time, and how much content is created. GitHub states:
  "Continuing to make requests while you are rate limited may result in the
  banning of your integration."

When several hosts use the same account, their calls add up against the same
limits. gh-paced keeps each host's share small enough that four hosts together
stay well below the documented numbers. It also reacts to GitHub's own signals
when the total gets close anyway.

## Installing and wiring it in

Build it and copy the binary somewhere on your PATH:

```bash
cargo build --release --manifest-path rs/Cargo.toml -p gh-paced
install -m 0755 rs/target/release/gh-paced ~/bin/gh-paced
```

gh-paced needs to know two things: which account's budget applies, and where the
real `gh` is.

- **Account.** `--account NAME`, or `$GH_PACED_ACCOUNT`. This is required.
  Budgets are kept separately for each account.
- **Real gh.** `--real-gh PATH`, or `$GH_PACED_REAL_GH`. Otherwise gh-paced uses
  `/usr/bin/gh`, then `/usr/local/bin/gh`. It never searches PATH. It refuses a
  path that resolves to gh-paced itself (exit 78), so a wrapper cannot loop.

Usually an existing account wrapper script hands every call to gh-paced as its
last step:

```bash
#!/bin/bash
# ~/bin/gh: select the account, then pace every call.
GH_PACED="$HOME/bin/gh-paced"
[ -x "$GH_PACED" ] || { echo "gh: $GH_PACED is missing; refusing to call GitHub unpaced" >&2; exit 127; }
exec "$GH_PACED" --account octocat --real-gh /usr/bin/gh -- "$@"
```

Everything after `--` is passed to gh exactly as given. git's credential helper
setting (`!gh auth git-credential`) goes through the same wrapper. That means
`git fetch` and `git push` are paced too, as GIT_CREDENTIAL calls. Such a call
never waits long, because git runs the helper while it may hold its caller's
locks: it is refused at once during a cooldown and waits at most
`GH_PACED_GIT_MAX_WAIT` (30 s) for its budget (see
[Waiting and refusing](#waiting-and-refusing)).

## What passes through unchanged

- **stdout** goes straight to the real gh's stdout, so pipes and `--json` output
  are untouched. The one exception is `gh api -i/--include`, whose stdout is
  copied through so its response headers can be read (see
  [pushback](#github-pushback-and-the-cooldown)).
- **stderr** is copied through while it is scanned for GitHub pushback (see
  below). When stderr is a terminal, gh sees a pseudo-terminal, so colours and
  prompts behave as before. A copied stream is scanned before it is passed on,
  and every byte gh-paced reads from gh reaches the consumer however slowly the
  consumer reads; gh feels the same back-pressure it would without gh-paced
  once 8 MiB is queued.
- **A consumer that goes away** (a closed pipe, as after `| head -n1` exits)
  is passed on to gh: gh's next write to that stream fails (EPIPE, and SIGPIPE
  by default; EIO through a pseudo-terminal) just as it would without
  gh-paced, so gh stops instead of writing on, and gh-paced reports gh's exit
  status or dies by gh's signal. Pushback already read from the stream still
  counts.
- **Output written after gh exits.** gh can leave a background process holding
  a copied stream open. After gh exits, gh-paced keeps reading until end of
  file, 2 s of silence, 5 s after the exit, or 64 MiB more, whichever comes
  first. The silence is counted from gh-paced's last read, so time it spends
  waiting for the state file's lock to record a cooldown counts too; before it
  stops at one of these limits it therefore reads, scans and delivers whatever
  can be read at once, up to 1 MiB, which is normally all that gh itself wrote
  before it exited. If the stream is still open then, gh-paced does not close it: it
  starts a copy of itself, `gh-paced --drain`, hands it the stream, and exits
  with gh's status at once. The drainer copies the rest of the stream to the
  same place until the last writer closes it or the consumer goes away, and
  keeps scanning stderr for pushback: a refusal there records the same
  cooldown and prints one `GH-PACED PUSHBACK` line. It holds no lock and no
  write slot. If gh-paced cannot find its own executable, or the drainer
  cannot start, the stream is closed instead, anything written to it later is
  lost, and gh-paced prints a warning saying so. See
  [Limitations](#limitations).
- **A signal after gh has exited** (INT, TERM, HUP or QUIT, sent to gh-paced
  or typed at its terminal, while gh-paced is still delivering gh's output)
  drops the output not yet delivered, nothing is handed to a drainer, and
  gh-paced dies by that signal promptly, even when the program reading its
  stdout or stderr has stopped reading. Pushback already read still counts.
  What gh wrote that gh-paced has not read yet is scanned for pushback but not
  delivered: whatever can be read at once, up to 1 MiB of each stream, at
  least what a pipe holds by default, so normally all that gh itself wrote
  before it exited.
  gh-paced waits for this normally well under a second, and at most
  `lock_wait_secs` plus 1 s, because a reader may first be waiting for the
  state file's lock. A header block on stdout that the signal cuts off counts
  as output cut short (see **Output cut short** below). Every
  message it prints after that (warnings, a PUSHBACK banner), however gh
  itself ended, is printed only if stderr accepts it within 2 s. A signal
  while gh is still running goes to gh instead (see the exit status
  below); if gh then exits while the reader is stalled, gh-paced goes on
  waiting for the reader, and a second signal ends it. A signal that arrives
  within a few milliseconds of gh's exit, before gh-paced has noticed the exit,
  has no effect; send it again. KILL ends gh-paced at once.
- **stdin** and the controlling terminal are inherited. The one exception: a
  write whose body arrives on stdin (`--body-file -`, `--input -`, `-F body=@-`,
  typed or in the expansion of one of gh's aliases) is read first so the
  content guard can check it, then replayed to gh byte for byte. A READ that
  sends stdin (`api -X GET ... --input -`) gets the inherited stdin itself,
  unread.
- **The editor.** For a WRITE, gh-paced sets `GH_EDITOR` so that gh opens your
  editor through gh-paced's editor guard, which checks the saved text before gh
  uses it (see [Text written in gh's editor](#text-written-in-ghs-editor)). You
  edit in the same editor as before.
- **Body files** named by a write (`--body-file`, `-F field=@file`, `--input
  FILE`, gist files, and so on) are copied to a private snapshot before
  anything inspects them, and gh is given the copy. What the guard checked is
  therefore exactly what gh sends, even if the original changes during a long
  pacing sleep. See [State](#state-the-audit-log-and-status).
- **The exit status** is gh's own. If gh is killed by a signal, gh-paced dies by
  the same signal, without a core dump. INT, TERM, HUP and QUIT sent to gh-paced
  are forwarded to gh. A signal that was ignored when gh-paced started (for
  example HUP under `nohup`) stays ignored in gh.
- gh-paced sets `GH_NO_UPDATE_NOTIFIER=1` and `GH_NO_EXTENSION_UPDATE_NOTIFIER=1`
  unless the caller set them, because the update checks are unpaced API calls.

gh-paced's own exit statuses are listed under [Exit status](#exit-status).

## Request classes

Each call gets exactly one class. Each class except LOCAL has its own token
bucket and hourly window. `gh-paced classify -- <args>` prints the class, the
cost, and the reason for any command line, without running it. It shows the
refusals decided from the command line, the configuration and gh's aliases
(every command, when gh's `config.yml` cannot be read); a real run can still
refuse for the body, the budget or the wait.

| Class | Covers | Cost |
| --- | --- | --- |
| LOCAL | `help`, `--version`, `--help` after one of gh's own commands, `completion`, gh's own `config` and `alias` subcommands (`config get/set/list/clear-cache`, `alias list/set/delete/import`), the help topics (`environment`, `formatting`, `reference`, ...), `auth token`, `auth switch`, `auth setup-git`, `auth git-credential store/erase` | free, not audited |
| READ | `gh api` with GET, HEAD or OPTIONS; `gh api graphql` whose query has no `mutation`; `pr view/list/status/checks/diff/checkout`; `issue view/list/status`; `run view/list/download/watch` (a watch needs `GH_PACED_ALLOW_WATCH=1`, see below); `workflow view/list`; `repo view/list/clone/set-default/gitignore/license`; `release view/list/download/verify`; the `list` and `view` forms of `label`, `gist`, `secret`, `variable`, `cache`, `ssh-key`, `gpg-key`, `org`, `project`, `ruleset`, `codespace`, `extension`; `auth status`; `browse`; `credits` | 1 |
| READ | `gh status` (it makes several GraphQL and REST read requests, more as notifications grow) | 10, the default READ burst |
| SEARCH | `gh search ...`, `gh extension search`, `gh api search/...` | 1 |
| WRITE | `gh api` with POST, PATCH, PUT or DELETE; `gh api` with any `-f`, `-F`, `--raw-field`, `--field` or `--input` and no explicit `-X GET` (gh sends those as POST); a GraphQL mutation, or a GraphQL request that cannot be inspected (a query on stdin, `--input -`, a nested `query[...]` field); `pr create/comment/edit/merge/close/reopen/review/ready`; `issue create/comment/edit/close/reopen/delete/transfer/lock`; `label`, `release`, `gist`, `secret`, `variable` changes; `workflow run`; `run rerun/cancel`; `repo create/edit/delete/fork`; **every command gh-paced does not recognise** (shell aliases, including one beneath one of gh's groups such as `issue publish`, extensions, new gh subcommands; an ordinary alias takes the class of its expansion), and the commands that hand their arguments to another program (`extension exec`, `copilot`), even with `--help`, because an alias or extension may turn that argument into anything | 1 |
| GIT_CREDENTIAL | `gh auth git-credential get`, which git calls before each network operation | 1 |

For a GraphQL call, every document gh might send is inspected: each `-f/-F
query=...` value (an `@file` is read) and, with `--input FILE`, the file's
`query` string and its raw text. The call is READ only when none of them
contains a mutation.

Some calls cost more than one token:

- `gh api --paginate` (or `--slurp`) fetches every page back to back. It costs
  10 tokens (`paginate_cost`) and prints a warning. This applies to writes and
  searches too, and since 10 is more than the WRITE burst (1) and the SEARCH
  burst (2), a paginated write or search is refused at the defaults (see the
  last item below). A paginated READ fits the READ burst of 10.
- `-L/--limit N` on a list or search costs one token per 100 items requested,
  rounded up, because gh fetches up to 100 items per request. At the default
  READ burst of 10, `-L 1000` is the largest limit a READ list can ask for, and
  `-L 200` the largest for a search.
- `gh status` costs 10 READ tokens: it makes several GraphQL and REST requests,
  more when there are many notifications, and 10 is the most one call can be
  charged at the default READ burst.
- Watch loops, `gh pr checks --watch` and `gh run watch`, keep polling for as
  long as they run, and gh-paced cannot count the requests each poll makes.
  They are **refused (exit 64) unless `GH_PACED_ALLOW_WATCH=1` is set**. The
  refusal suggests the paced alternative: run `gh pr checks <pr>` or
  `gh run view <run>` about once a minute in a loop, each call paced on its
  own. With the opt-in, a watch costs 20 tokens up front (`watch_cost`), and
  the tokens buy a fixed run time: `floor((cost - 2) / requests per poll) x
  interval` seconds. 2 tokens pay for startup (finding the run or pull
  request); a `gh run watch` poll is charged 4 requests (gh fetches the run,
  its workflow and its jobs, and more for large or failing runs) and a
  `gh pr checks --watch` poll 2. Both are estimates: a large or failing run
  can make more requests than its poll is charged (see
  [Limitations](#limitations)). At 30 s intervals that is 120 s for
  `run watch` and 270 s for `pr checks --watch`. A watch still running at that
  deadline is stopped (TERM, then KILL 5 s later) and gh-paced exits 75. A
  polling interval shorter than 30 s is refused (exit 64) unless
  `GH_PACED_ALLOW_FAST_WATCH=1` is set. Pass `--interval 30` instead. Every
  `--interval` value must be a plain positive whole number of seconds (`30`,
  not `0`, `-1`, `030` or `0x1e`); anything else is refused (exit 64) even
  with the override, because gh treats zero or a negative interval as no sleep
  at all.
- **A call costing more than its class's burst is refused at once** (exit 75),
  before gh runs, unless it is a watch. One gh call makes all the requests its
  cost stands for back to back, and gh-paced cannot space out requests inside
  one gh call, so no amount of waiting would keep such a call within the burst.
  When the account-wide budget is low and the burst is halved (see
  [Account-wide feedback](#account-wide-feedback-get-rate_limit)), the halved
  burst is the limit: a paginated READ (10) is refused while the READ burst is
  5. The message suggests a smaller `--limit`, dropping `--paginate`, smaller
  calls, or a larger burst in the config file.
- A call whose cost is larger than its class's hourly cap can never be
  admitted, so it is refused at once (exit 75) rather than after a wait.

When a flag that sets a cost or a limit is repeated (`--interval`, `-L`), the
most conservative value counts, whichever occurrence gh honours. Short flags
are read the way gh reads them, including groups: `pr list -dL1000` is
`-d -L 1000`.

gh-paced does not know which of gh's flags take a value, so it reads every
word that could be a cost or limit flag, and errs toward charging more. A `--`
can be another flag's value (`--label -- --limit 1000` sets the label to `--`
and the limit to 1000), so gh-paced reads past every `--` and charges that call
10. The other side of the same rule: a value that looks like a limit flag is
read as one. `pr list --label -L50001` (a label named `-L50001`) is charged 501
tokens and refused. Write such a value with `=` (`--label=-L50001`), which
gh-paced reads as one word.

## Budgets and the GitHub limits behind them

Each budget is a **token bucket** plus a **sliding one-hour cap**. The bucket
holds up to `burst` tokens and refills at `per_minute`. A call is admitted when
the bucket holds enough tokens *and* the cost fits under `per_hour` in the last
3,600 seconds. A call costing more than the burst is refused (see
[Request classes](#request-classes)), with one exception: a watch loop
(`GH_PACED_ALLOW_WATCH=1`), whose requests are spread over its polling
interval, is admitted on a full bucket and charged its full cost, which leaves
the bucket in debt. Later calls wait until the debt is repaid.

Default budgets, per host, per account:

| Class | per_minute | burst | per_hour | Other |
| --- | --- | --- | --- | --- |
| READ | 20 | 10 | 500 | |
| SEARCH | 5 | 2 | 150 | |
| WRITE | 2 (one per 30 s) | 1 | 30 | at most 1 write in flight per host |
| GIT_CREDENTIAL | 6 (one per 10 s) | 1 | 120 | |

### What GitHub documents

From [Rate limits for the REST API](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api):

- "All of these requests count towards your personal rate limit of 5,000
  requests per hour."
- "No more than 100 concurrent requests are allowed. This limit is shared across
  the REST API and GraphQL API."
- "No more than 900 points per minute are allowed for REST API endpoints, and no
  more than 2,000 points per minute are allowed for the GraphQL API endpoint."
  Most GET, HEAD and OPTIONS requests cost 1 point; most POST, PATCH, PUT and
  DELETE requests cost 5.
- "No more than 90 seconds of CPU time per 60 seconds of real time is allowed."
- "In general, no more than 80 content-generating requests per minute and no
  more than 500 content-generating requests per hour are allowed. Some endpoints
  have lower content creation limits."
- "These secondary rate limits are subject to change without notice. You may
  also encounter a secondary rate limit for undisclosed reasons."
- On a secondary limit: "If the retry-after response header is present, you
  should not retry your request until after that many seconds has elapsed. ...
  Otherwise, wait for at least one minute before retrying."
- "Continuing to make requests while you are rate limited may result in the
  banning of your integration."

From [the search endpoints](https://docs.github.com/en/rest/search/search):
"30 requests per minute for all search endpoints except for the Search code
endpoint. The Search code endpoint requires you to authenticate and limits you to
10 requests per minute."

From [Rate limits and query limits for the GraphQL API](https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api):
"For users: 5,000 points per hour per user."

From [the rate limit endpoint](https://docs.github.com/en/rest/rate-limit/rate-limit):
"Accessing this endpoint does not count against your REST API rate limit." The
REST guide adds that it "can count against your secondary rate limit".

### Four hosts on one account

| Class | Four hosts at the default budget | GitHub's documented limit | Share |
| --- | --- | --- | --- |
| READ per hour | 4 x 500 = 2,000 gh calls | 5,000 requests (REST) | 40% |
| READ per minute | 4 x 20 = 80 | 900 points per endpoint | 9% |
| SEARCH per minute | 4 x 5 = 20 | 30 | 67% |
| WRITE per minute | 4 x 2 = 8 | 80 content-generating | 10% |
| WRITE per hour | 4 x 30 = 120 | 500 content-generating | 24% |
| Concurrent writes | 4 x 1 = 4 | 100 concurrent requests (all kinds) | 4% |

Two caveats keep these numbers from being a guarantee:

1. **One gh call can make several HTTP requests.** For example, `pr view` with
   many `--json` fields, or `pr checks`, can make more than one. At two requests
   per call, four hosts at their full READ budget would make 4,000 requests an
   hour, 80% of the primary limit. The account-wide feedback below exists for
   this case.
2. **gh-paced sees only calls that go through it.** A browser session, the web
   UI, GitHub Actions running as the same user, or a script with its own HTTP
   client all draw on the same limits.

The WRITE budget is the strictest. GitHub's content-creation limit is the one
with undisclosed lower per-endpoint values, and content written by a suspended
account is hidden.

## Account-wide feedback: `GET /rate_limit`

Each host's budget is local. To see the whole account's position, gh-paced
regularly runs `gh api rate_limit` and caches the result in the state file. That
endpoint is free on the primary limit, but it counts against secondary limits,
so it is paced too:

- **When it runs.** A refresh is due when there is no snapshot, the snapshot is
  at least 300 s old (`rate_limit_refresh_secs`), or 50 calls have been admitted
  since the last one (`rate_limit_refresh_calls`). It never runs more often than
  once every 60 s (`rate_limit_min_refresh_secs`), never during a cooldown, and
  only one process on the host runs it at a time.
- **Cost.** One READ token, charged under the lock before the request is sent,
  so a refresh can never push the host past its READ budget. When a refresh is
  due and the READ bucket or hourly cap has no room for that token, READ,
  SEARCH and WRITE calls wait for room (or are refused past
  `GH_PACED_MAX_WAIT`) rather than run without current account numbers. The
  refresh gives up after 20 s.
- **After a sleep.** Every admission pass re-reads the state, so a call that
  slept (for a bucket, a cooldown, or a write slot) checks the account-wide
  numbers again, refreshing first if a refresh has come due, before it is
  admitted.
- **Halving.** If the tightest relevant resource has 50% or less remaining
  (`halve_below_fraction`), READ, SEARCH and WRITE run at half their per-minute
  rate, burst and hourly cap. READ and WRITE look at `core` and `graphql`;
  SEARCH also looks at `search`.
- **Blocking.** At 20% or less remaining (`block_below_fraction`), every READ,
  SEARCH and WRITE call waits until that resource resets. If the reset is
  further away than `GH_PACED_MAX_WAIT`, the call is refused (exit 75).
  GIT_CREDENTIAL calls are not blocked: they do not use the API pools.
- **Staleness.** A snapshot older than one hour is ignored. A resource whose
  reset time has passed counts as full. Any pushback (next section) discards the
  snapshot so the next call fetches a fresh one.

Between two refreshes on one host, four hosts can spend at most about
4 x 20 x 5 = 400 READ tokens (8% of 5,000), or 200 when halved. The 20% blocking
floor therefore leaves room for calls that land before the next refresh.

## GitHub pushback and the cooldown

gh prints server refusals on stderr. gh-paced scans that stream as it passes
through, keeping a 4 KiB overlap between reads so a phrase split across two
reads is still found. It looks for:

- `secondary rate limit`, `rate limit` (including `API rate limit exceeded`),
  `HTTP 429`, the word `abuse`, `submitted too quickly`;
- `HTTP 403` with none of the above;
- a `Retry-After: N` value (the largest one seen). It is honoured in full up
  to 31,536,000 s (365 days). A longer value, or one too long to represent, is
  never shortened: it starts a cooldown with no end time, every paced call is
  refused (exit 75) with a message saying so, and the cooldown ends only when a
  person removes `<account>.cooldown` and the `cooldown` entry of
  `<account>.json` from the state directory (see
  [Ending a cooldown by hand](#ending-a-cooldown-by-hand)). A value whose
  digits run on past the 4 KiB kept between reads cannot be read whole, and is
  treated the same way. So is one that starts its line (after blanks, or after
  the `<` marker of gh's debug output) and runs up to the end of the first
  1 MiB of stderr, which is all that the rate-limit refresh (`gh api
  rate_limit`, run by gh-paced itself) keeps; the refresh prints a warning when
  it cut either stream there.
  The same words in the middle of a line of prose, cut there, are not a header
  and start no such cooldown.

For `gh api -i/--include`, gh prints the HTTP status line and response headers
on **stdout** instead. For that form only, gh-paced also reads stdout as it
passes through, looking only inside header blocks: a 403 or 429 status,
`Retry-After`, and `X-RateLimit-Remaining: 0`. gh prints a header block as an
`HTTP/<version> <status>` line, then header lines ending in CRLF, then a CRLF
blank line. stdout itself is passed on unchanged.

- **One response (no `--paginate`).** gh prints exactly one header block, and
  it is the first thing on stdout. gh-paced reads that first block and nothing
  after it, so a response body cannot start a cooldown, whatever it contains.
  The block counts even if it is cut off before its blank line (the stream
  ended, or a line of another shape followed); see "Output cut short" below for
  what a block cut by the end of stdout keeps.
- **`--paginate`.** gh prints one header block per page, each after the
  previous page's body, so gh-paced reads every block it finds in the shape
  above. A block counts when its CRLF blank line arrives, or when the output
  ends after at least one of its CRLF header lines, or when the output ends
  inside a block that began on the first stdout line. A later page's block that
  the output cuts before its first CRLF header line is complete may be a
  `--jq` page body cut short, so it does not count, and the cooldown is
  whatever stderr gives (900 s for an `HTTP 429`). Body lines that merely look
  like a status line and headers, such as lines printed through `--jq`, end in
  a bare LF and are not treated as a header. A page body that itself contains
  that exact shape with CRLF line endings is read as one more header block and
  can start a cooldown that was not needed. gh-paced cannot tell such a body
  from a real page boundary, so this errs on the safe side.
- **Over-long lines.** A stdout line of more than 8,192 bytes before its line
  end is not kept. In a
  403 or 429 block, such a line (a `Retry-After` with thousands of leading
  zeros, or any other header that long) means the wait GitHub asked for cannot
  be read, so it starts a cooldown with no end time, as for a `Retry-After`
  over 365 days above, never a shorter one. In any other block it ends the
  block. A status line that long (HTTP/1.1 lets the server choose its reason
  phrase) still opens its block. With `--paginate` a long line needs a CRLF
  ending to count. Before the block's first CRLF header line it also needs the
  shape a short header line has: a colon in its first 8,192 bytes. A long line
  there without one is body text and drops the block. After a CRLF header line,
  a long line counts whether or not its colon is in the bytes kept, so a
  `Retry-After` already read is never dropped. If the output ends inside such a
  long line of a 403 or 429 block (one with the colon, or one after a CRLF
  header line), before its line ending arrives (gh stopped mid-write), the
  block still counts, with no end time, when it began on the first stdout line
  or already holds a CRLF header line. A later page's block
  cut inside its first header line may be a `--jq` page body cut short, so it
  does not count, and the cooldown is whatever stderr gives (900 s for an
  `HTTP 429`).
- **Output cut short.** stdout can stop at any byte. gh can be killed, and when
  the terminal or the program reading stdout goes away, gh-paced stops reading
  after the chunk it holds; a terminal can hand over a chunk that ends in the
  middle of a header line, even when gh wrote that line in one piece. So a
  header line that the output ends without its line ending is not trusted:
  `Retry-After: 17` may be the start of `Retry-After: 172800`. If the output
  ends inside a 403 or 429 block that counts (see above) before a whole
  `Retry-After` line (one with its line ending) arrived, the wait GitHub asked
  for is not known, and the cooldown has no end time, as for a `Retry-After`
  over 365 days above, never a shorter one. With one response, a cut
  `Retry-After` line in a block of any other status does the same; with
  `--paginate`, a cut last line that is not the first line of stdout is not
  read. A `Retry-After` line that arrived whole is kept
  even when the block is cut later. With one response, a last line that ends
  in CR is whole: only its LF is missing. A cut before the status code arrives
  leaves nothing on stdout to read, so stderr decides, as for a call without
  `-i`. So `gh api -i ... | head -n1` on a 403 or 429 can start a cooldown
  with no end time, even for a 403 that was only a permission error. Once you
  know it was not a limit, end it as described in
  [Ending a cooldown by hand](#ending-a-cooldown-by-hand).

Any limit signal (one of the phrases, HTTP 429, a `Retry-After` value, or
`X-RateLimit-Remaining: 0`) starts a **cooldown** of the larger of `Retry-After`
and 900 s (`cooldown_secs`). A plain `HTTP 403` starts a cooldown of
`plain_403_cooldown_secs`, also 900 s. Neither can be configured below 900 s:
GitHub does not always say that a 403 is a rate limit, so every 403 is treated
as one. During a cooldown, every paced call for that account on that host waits
or is refused. A new cooldown only ever extends an existing one. gh's own output
and exit status still pass through. The cooldown is recorded as soon as the
signal is read from gh's output, before that output is passed on, so other
calls on the host pause at once even if the program reading this call's output
is slow or has stopped reading. The banner below is printed after gh's output
has been passed on. Pushback that a `gh-paced --drain` process reads from
stderr after gh-paced has exited (see
[What passes through unchanged](#what-passes-through-unchanged)) records the
same cooldown and prints a one-line `GH-PACED PUSHBACK` message, but it is not
written to the audit log. A cooldown with no end time ends only when a person
ends it; see [Ending a cooldown by hand](#ending-a-cooldown-by-hand).

The banner looks like this:

```text
GH-PACED ********************************************************************
GH-PACED PUSHBACK [octocat] GitHub refused or throttled `api GET repos/o/r/issues/7/comments`: secondary rate limit, HTTP 403
GH-PACED PUSHBACK [octocat] every paced gh call for this account on this host is paused for 900 s, until 3:51 AM ET
GH-PACED PUSHBACK [octocat] STOP making GitHub calls and tell your coordinator. Do not retry in a loop. Never switch accounts to continue.
GH-PACED ********************************************************************
```

Apart from `--include` header blocks, gh-paced does not scan stdout. Error text
from GitHub arrives on stderr, and stdout often carries the caller's data
(`--json` output, `gh api` responses) that may legitimately contain these words.

### Ending a cooldown by hand

A cooldown with an end time ends by itself; wait for it. A cooldown with no end
time (a `Retry-After` over 365 days, or a wait gh-paced could not read whole)
lasts until a person ends it. End it only after checking with GitHub that the
account is no longer limited, or once you know the refusal was not a limit (a
403 for a missing permission, for example).

The cooldown is kept twice in the state directory: in `<account>.cooldown` and
in the `cooldown` entry of `<account>.json`. Every load takes the later of the
two, so both must be cleared. `gh-paced status --account NAME`, run with the
same `GH_PACED_STATE_DIR`, `XDG_STATE_HOME` and `HOME` as your gh shim, prints
the state file's path on its `state file:` line, and the refusal message names
both files. Set `state` to that path and run this. It holds the account's lock
(`<account>.lock`, the lock every gh-paced process takes) while it removes
`<account>.cooldown` and sets the entry to `null`, so no paced call reads or
rewrites the state halfway through:

```sh
state="$HOME/.local/state/gh-paced/octocat.json"
flock -w 30 "${state%.json}.lock" python3 - "$state" <<'EOF'
import json, os, sys, tempfile
state = sys.argv[1]
try:
    os.remove(state[:-len(".json")] + ".cooldown")
except FileNotFoundError:
    pass
with open(state) as f:
    s = json.load(f)
s["cooldown"] = None
d, name = os.path.split(state)
fd, tmp = tempfile.mkstemp(dir=d, prefix="." + name + ".clearing-")
try:
    os.fchmod(fd, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(s, f, indent=2)
    os.replace(tmp, state)
except BaseException:
    os.unlink(tmp)
    raise
EOF
```

If `flock` gives up after 30 s, a gh-paced process is holding the lock; run the
command again. Then run `gh-paced status --account NAME` again: it prints
`cooldown: none`, and paced calls are no longer refused for the cooldown. What
the command leaves behind:

- No `<account>.cooldown` file (the next pushback writes a new one), and
  `"cooldown": null` in `<account>.json`, readable and writable by you alone
  (mode 0600) whatever your umask. The new content is written to a fresh
  `.<account>.json.clearing-<random>` file beside it and renamed over
  `<account>.json` at the end. If a step fails with a Python error, the
  command removes that file and `<account>.json` is unchanged. If the command
  is killed (TERM, KILL, or a closed terminal) before the rename, the file can
  be left behind with `<account>.json` unchanged; gh-paced ignores such a file,
  and it is safe to delete.
- Everything else in `<account>.json` as it was: the buckets and hourly windows
  (the calls of the last hour still count), the in-flight writes, and the
  refresh bookkeeping. The cached `GET /rate_limit` numbers were normally
  dropped when the call that met the pushback finished, so the next refresh
  fetches new ones (see
  [Account-wide feedback](#account-wide-feedback-get-rate_limit); at most one
  every `rate_limit_min_refresh_secs`, 60 s by default).
- The audit log as it was, with its `pushback` and `refuse` records. The
  clearing is not recorded: gh-paced does not see it.

Do not remove only one of the two: removing `<account>.cooldown` alone leaves
the entry in force, and clearing the entry alone brings the cooldown back from
`<account>.cooldown` at the next load. Do not delete `<account>.json` instead:
with no state file the next call starts from full buckets and forgets the calls
of the last hour. A call that met the pushback and is still running records
the cooldown again when it finishes, and a `gh-paced --drain` process still
copying a call's stderr records one again if it reads another limit signal, so
clear the cooldown after that call has exited, and check `status` afterwards.

## The write content guard

GitHub issues, comments and pull-request descriptions are for short notes that
people read. They are not a place to store logs, archives or encoded data.
Before running a WRITE, gh-paced reads every body source:

- `pr`/`issue` `create`, `comment`, `edit`, and `pr review`: `--title`,
  `--body`, `--body-file` (including `-` for stdin), and the short forms;
- `pr merge`: `--subject`, `--body`, `--body-file`;
- `pr`/`issue` `close` and `reopen`: `--comment`;
- `release create/edit`: `--title`, `--notes`, `--notes-file`;
- `gist create` (its files and `--desc`) and `gist edit` (`--add`, `--desc`);
- `label create/edit` and `repo create/edit`: the name and description;
- `secret set`/`variable set`: `--body`, `--env-file`, or stdin;
- `gh api -f/--raw-field`, `-F/--field` (including `@file` and `@-`), and
  `--input FILE` (including `-`). A JSON request file counts at its full size,
  and every string inside it is also scanned after JSON decoding, so a `body`
  field is caught however it is escaped;
- `gh workflow run -f/-F` and `--json` (stdin);
- for a command gh-paced cannot see into (an extension, a shell alias, an
  unknown command, a word beneath one of gh's groups that is not one of gh's
  commands, and `extension exec` and `copilot`, which hand their arguments to
  another program), every argument, flag-shaped or not (`--payload=<text>`,
  `--repo <text>`, `-- <text>`), and every occurrence of a repeated flag
  rather than only the last, because that program may pass any of them to a
  body flag;
- for one of gh's own aliases, whatever its expansion sends: gh-paced expands
  the alias itself and runs gh with the expansion (see
  [gh's own aliases](#ghs-own-aliases)), so the expansion is checked, and its
  body files snapshotted, exactly as if it had been typed;
- text typed in the editor gh opens for a write (see
  [Text written in gh's editor](#text-written-in-ghs-editor));
- for any other write, `--body`, `--body-file` and `--input`.

Every body file is first copied to a private snapshot (see
[State](#state-the-audit-log-and-status)), and the guard and gh both read the
copy. A file that cannot be copied (missing, unreadable, or larger than 64 MiB,
a cap that applies even with `GH_PACED_ALLOW_LARGE_BODY=1`) is refused with
exit 65.

It refuses the call (exit 65), before anything reaches GitHub, when either
condition holds:

- the sources total more than **8,192 bytes** (`max_body_bytes`; a config file
  may lower it to as little as 256, never raise it);
- any source contains **base64-looking content longer than 1,000 characters**
  (`max_base64_run`; a config file may lower it to as little as 100, never raise
  it).

Base64-looking content is either of:

- one unbroken run of base64-alphabet characters (letters, digits, `+`, `/`,
  `=`, `_`, `-`), whatever it contains. A long hex dump, a lower-case-only
  encoding, and a long ruler of dashes all count, so nothing long slips through
  as a "word";
- a block of consecutive lines, each at least 20 characters and made only of
  base64-alphabet characters, counted together, whatever they contain. The
  usual 64- and 76-column wrapping therefore does not hide an encoding;
- a block of consecutive base64-alphabet lines of any width that all have the
  same width, except that the last may be shorter, counted together. This is
  the shape of an encoding wrapped at a fixed column, so wrapping at 16 or 8
  columns does not hide one either. Lines of differing widths, such as a list
  of test names one per line, do not form this kind of block;
- a block of consecutive base64-alphabet lines, each at least 4 characters and
  each holding at least one upper-case letter or made of one repeated
  character, of any widths, counted together. Standard base64 has an
  upper-case letter on nearly every line (and the encoding of zero bytes is
  all `A`), so an encoding wrapped at varying narrow widths (16, then 12, then
  16) forms this block, and so does a run of `/` (bytes 0xFF). A list of
  lower-case identifiers or hex hashes one per line does not. A list of
  mixed-case or upper-case names one per line (`CamelCaseTestName`) does,
  once it passes the limit. A line that has no upper-case letter and is not
  made of one repeated character ends this block. So an encoding wrapped at
  varying widths under 20 columns whose lines lack an upper-case letter
  (lower-case base32 or hex, or base64 that happens to have none, such as
  `abcd` repeated) is not counted by any of these shapes. Lines that are each
  one repeated character (`aaaa`), and upper-case base32 or hex, are counted.

Lists can match these shapes too. Bare commit SHAs one per line are refused at
26 or more full 40-character SHAs, or 143 or more 7-character short SHAs; a
column of 4-digit numbers one per line is refused at 251 lines; 59 or more
17-character mixed-case names one per line are refused. Put a word on each line
(`<sha> fix the parser`), or point at a commit range, instead.

These shapes are a guard against an accidental upload, not a proof. An encoding
broken up by spaces or punctuation is not seen, and neither is one wrapped at
varying widths in lines of under 4 characters, or one wrapped at varying widths
under 20 columns whose lines lack an upper-case letter (lower-case base32 or
hex, say), unless each such line is one repeated character.

The refusal names every reason that applies. Files are read only up to the
limit plus one byte, so a very large file is reported as "at least" that size:

```text
GH-PACED REFUSED [octocat] content guard: write body is 9000 bytes across 1 source(s), over the 8192-byte limit
GH-PACED REFUSED [octocat] not running `issue comment` (exit 65)
GH-PACED REFUSED [octocat] GitHub text is for short human notes. Keep evidence on the host and post a pointer (path + sha256, a tracked file, or a commit).
```

`GH_PACED_ALLOW_LARGE_BODY=1` skips the content guard for one call (the size and
base64 checks, and the refusals in the next subsection; the 64 MiB snapshot cap
still applies). Use it only for genuinely human-written long text, such as a
long design comment, and never for encoded or machine-generated payloads.

### gh's own aliases

gh expands an ordinary alias (the `aliases:` section of gh's `config.yml`)
inside its own process: with `upload: api -X POST repos/o/r/issues/$1/comments
--input -`, `gh upload 7` sends stdin as a comment, and with `w: run watch 99`,
`gh w` is a watch loop. So gh-paced reads gh's aliases the way gh does, expands
the alias (`$1`-style placeholders, leftover arguments appended, aliases of
aliases followed up to 5 deep), and runs gh with the expanded command line.
Classification, budgets, the watch rules, the content guard and the snapshots
all apply to the expansion, and gh is given the expanded command line instead
of the alias name. `gh-paced classify` shows the expansion on an `alias:`
line. gh reads its configuration again when it runs; see
[Limitations](#limitations) for what that leaves open.

gh's configuration file is found the way gh finds it: `$GH_CONFIG_DIR`,
`$XDG_CONFIG_HOME/gh`, or `$HOME/.config/gh` (a path relative to the working
directory when `HOME` is empty). A missing or empty file means gh's single
default alias, `co: pr checkout`. An alias name is placed the way gh adds it:
beneath gh's own groups only (`issue publish`, `cs mine` beneath `codespace`),
never in place of one of gh's commands, and it is found after flags gh skips
while looking for a command (`gh issue -R o/r publish`). A quoted last word
with a space in it is invoked by its first word, as gh names the command
(`"issue 'publish now'"` runs as `gh issue publish`). An installed extension
wins over a root alias of the same name, as in gh: the extension runs.

A shell alias (`!...`) is passed to gh by name and charged one WRITE token; gh
runs it with `sh -c`, and any `gh` it runs is paced only if that `gh` resolves
to gh-paced (see [Limitations](#limitations)).

When gh-paced cannot be sure which command gh would run, it refuses instead of
guessing:

- gh's `config.yml` cannot be read, is larger than 1 MiB, or uses YAML that
  gh-paced does not read (tags, anchors, merge keys, nested collections, tabs
  in the indentation, a key set twice, a byte order mark after the first
  character, the NEL, LS or PS line separators, control characters, a `---`
  or `...` document marker after the first setting), or one of `GH_CONFIG_DIR`,
  `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `HOME` is not UTF-8: every command
  line that names a command is refused with exit 78, gh's own commands and
  installed extensions included, because any of them can be an alias in that
  file (gh adds `help` after the aliases, a quoted name such as `"'pr x'"`
  gives gh a second `pr`, and an alias named like an extension runs when gh
  registers no extensions). Only a line naming no command, such as
  `gh --version`, still runs. That includes `gh auth git-credential`, so git
  cannot use gh as its credential helper until the file or variable is fixed;
- the position of the alias name depends on whether an unfamiliar flag before
  it takes a value (`gh issue --some-flag publish`): exit 64; put the alias
  name right after the command words;
- two aliases with the same name and different expansions, an alias whose
  name gives gh a second command of the same name (`"issue 'view all'"`
  beside `gh issue view`, or `"'myext now'"` beside an extension `myext`), an
  alias named like an installed extension that gh might not register (gh
  registers no extension when one entry of its extensions directory cannot be
  read; gh-paced is sure only when every `gh-*` entry is a directory without a
  `manifest.yml` or a symbolic link), any root alias when the extensions
  directory cannot be listed, an alias named `help`, an alias beneath a word
  gh-paced does not know as one of gh's commands, aliases nested more than 5
  deep or in a loop, or an expansion gh would reject for these arguments (a
  `$2` with one argument, an open quote): exit 64. The refusal names the alias
  but not its expansion, which can hold a body.

### Bodies gh composes itself

Some write forms make gh build the body after gh-paced has run, from a source
neither the argument check nor the editor guard sees. These are refused with
exit 65 unless `GH_PACED_ALLOW_LARGE_BODY=1` is set, which skips the whole
content guard:

- `pr create` / `issue create` with `--template`/`-T`, `--fill`,
  `--fill-first`, `--fill-verbose`/`-f`, or `--editor`/`-e`;
- `pr comment` / `issue comment` with `--editor`/`-e`;
- on a terminal, these forms that prompt for the body: `pr create` without
  `--body`/`--body-file` (or `--web` or `--recover`), because it offers a body
  composed from the branch's commit messages and submits it without opening an
  editor if the author just presses Enter; `pr comment` / `issue comment`
  without `--body`/`--body-file` (or `--web` or `--delete-last`); `pr review` without
  `--approve`, `--request-changes`, `--comment` or a body; `pr edit` /
  `issue edit` with no flags; `release create` without `--notes`,
  `--notes-file` or `--generate-notes`;
- `release create --notes-from-tag` (gh reads the tag's annotation or commit
  message after gh-paced has run and sends it as the release notes);
- `gist edit` without `--add` or `--remove` (it opens an editor or replaces a
  file gh reads later).

Two interactive forms are no longer refused, because the text the user writes
reaches gh only through the editor, which the editor guard checks: an
interactive `issue create` (its body) and an interactive `pr merge` (the merge
commit message, when the user chooses to edit it).

Boolean flags count by their value, as gh reads them: `--editor=false` is not
the editor form, and `--web=false` is not the web form.

The fix is always to write the text first and pass it with `--body` or
`--body-file`.

### Text written in gh's editor

For every WRITE, gh-paced sets `GH_EDITOR` (which gh prefers over every other
editor setting) to `gh-paced --edit-guard <account>`, and puts the editor you
would otherwise have had in `GH_PACED_EDITOR`. That is gh's own choice, in
gh's order: `GH_EDITOR`, the `editor` key of gh's `config.yml`, `GIT_EDITOR`,
`VISUAL`, `EDITOR`, then `nano`. When gh opens an editor, it runs the guard,
which runs your editor on the same file, waits for it, and then checks the
saved text with the same size and base64 limits as any other body. The size
limit is shared with the command line: gh-paced passes the number of body bytes
the arguments, files and stdin already used (for example a `--title`) to the
guard in `GH_PACED_BODY_BYTES_USED`, and the text saved in the editor (every
file gh opens in that editor run, together) is checked against what is left.
It is not one allowance for everything the call sends: a title or other answer
gh prompts for on the terminal is not counted, and if gh opens the editor more
than once in one call, each opening is checked against what the command line
left, not against what an earlier opening used. A value in
`GH_PACED_BODY_BYTES_USED` that is not a byte count is refused (exit 78). If
the text passes, gh carries on as usual. If it fails, the guard prints a `GH-PACED REFUSED`
banner, keeps a private copy of the text in the state directory as
`<account>.refused-edit.<nanoseconds>.md` (gh deletes its own file; at most the
first 1 MiB is kept, and the banner says when the rest was not copied), and
exits 65. gh then abandons the command, sends nothing, and exits with its own status
(usually 1), not 65.

A gh-paced started inside another one's gh keeps the outer guard rather than
wrapping it again. If gh-paced cannot find its own executable, the editor is
replaced by a command that refuses with exit 65, because the text could not be
checked.

What the editor guard does not cover is listed under
[Limitations](#limitations).

## Waiting and refusing

When a call cannot be admitted yet, gh-paced works out how long to wait. That is
the longest of: the cooldown, the account-wide block, a free write slot, the
bucket refill, and the hourly cap. It then prints one warning and sleeps:

```text
GH-PACED WARNING [octocat] write budget 1 per 30 s (burst 1) per host reached; sleeping 28 s (next slot 3:36 AM ET)
```

If the total time slept plus the next wait would exceed `GH_PACED_MAX_WAIT`
(default 900 s), the call is refused instead. Nothing is sent to GitHub:

```text
GH-PACED ********************************************************************
GH-PACED REFUSED [octocat] write budget 30/hour per host reached (30 used in the last hour)
GH-PACED REFUSED [octocat] the next slot is in 1712 s (at 4:05 AM ET), beyond GH_PACED_MAX_WAIT=900 s
GH-PACED REFUSED [octocat] not running `issue comment` (exit 75)
GH-PACED ********************************************************************
```

A GIT_CREDENTIAL call (`gh auth git-credential get`, run by git as its
credential helper) has a shorter bound, because git runs it inside a fetch or
push that may hold its caller's locks for as long as the helper sleeps:

- during a cooldown (or the pause after state recovery) it is refused at once,
  without sleeping, with one line that names the cause and the seconds left:

  ```text
  GH-PACED REFUSED [octocat] cooldown after GitHub pushback (HTTP 403) on `api GET rate_limit`; 842 s left (ends 9:19 PM ET); a git credential helper never waits out a cooldown, so git fails now instead of holding its caller's locks; not running `auth git-credential get` (exit 75)
  ```

- otherwise it waits for its budget for at most `GH_PACED_GIT_MAX_WAIT`
  (default 30 s; the smaller of it and `GH_PACED_MAX_WAIT` applies), counting
  time spent waiting for the state lock as well as sleeps, measured from the
  start of the call on a monotonic clock that keeps counting while the machine
  is suspended (setting the system time back does not lengthen it), and is
  refused beyond it with the banner above naming `GH_PACED_GIT_MAX_WAIT`. Each
  of its sleeps ends at a deadline on that clock, never past the bound, so a
  stall or a suspend before or during the sleep does not lengthen it either; if
  the system refuses that timer, the call stops with exit 70 and a message
  naming `GH_PACED_GIT_MAX_WAIT` instead of sleeping. A call that becomes
  admissible only after the bound has passed, because a sleep ended late or the
  machine resumed from a suspend past it, because the bound passed before a
  sleep began (it then does not sleep), or because it got the state lock only
  then, is refused the same way (exit 75), without using the budget. A call
  that waited for nothing is admitted however long its own work took, so a
  bound of 0 still runs a call whose budget and state lock are free.
- it waits for the state lock only for what is left of that bound, not the
  whole `GH_PACED_LOCK_WAIT`: the bound counts from the start of the call, so
  this holds before gh runs (recording a refusal of one of gh's aliases
  included) and after it; a lock not free in time exits 70 with a message naming
  both variables. When the bookkeeping after gh exits cannot get the lock in
  time, the message says so and gh's own exit status stands, without `quit=1`,
  since gh may already have answered git. The exception is recording a cooldown
  that gh's output showed, which waits the whole `GH_PACED_LOCK_WAIT` as for
  every class, so that the cooldown is not lost.

Either refusal, and any other failure before gh starts (exit 70, 75 or 78: a
busy state lock, nesting too deep, a configuration error; or 127, when gh's
program could not be executed at all), also prints `quit=1`
on stdout. That is git's credential
protocol for "stop now": git ends the operation with `fatal: credential helper
'...' told us to quit` instead of trying another helper or prompting for a
password on a terminal. The cooldown, the budgets and every other class's
waits are unchanged; nothing is sent to GitHub.

Writes are also limited to `write.max_in_flight` (default 1) at a time per host.
A write that finds the slot taken checks again every 5 s and prints its warning
on every check, so a waiting write is never silent.

The slot lasts as long as the gh process doing the write, not as long as the
wrapper. An admitted write creates a lease file, `<account>.lease-<nonce>` in
the state directory, takes an exclusive `flock` on it, and passes that open file
to gh. The kernel keeps the lock while any process still has the file open: the
wrapper, gh, or anything gh started. So if the wrapper is killed with SIGKILL,
the gh it started keeps the slot until gh itself exits. Other processes test a
slot with a non-blocking shared `flock`; a lease that is no longer locked is
cleared, and so is a holder from an older state file whose process has died.

When the write ends but a process gh started still holds the lease (gh left a
background helper running), gh-paced closes its own copy and leaves the slot
taken; a later write clears it once that process has exited.

A gh-paced started inside a gh run by another gh-paced (for example by an alias
or extension that calls gh again) does not wait for its own parent's slot. It
must prove that it descends from the holder in two ways: the holder's nonce is in
`GH_PACED_INFLIGHT_CHAIN`, and it holds the holder's locked lease file, inherited
from the holder. The second is checked in `/proc/self/fdinfo`, where Linux lists
the `flock` only for the open file that took it, so opening the lease file
again (for example redirecting stdin from it) proves nothing, and neither does
setting the variable by hand. Nesting deeper than 8 levels is refused (exit 75).

A process counts as running only while its PID and start time both match and
it is not a zombie.

Caveats of the lease design, all of which err toward fewer calls except the
second:

- a long-lived background process that gh starts and that keeps the inherited
  file open keeps the slot taken until it exits; later writes wait and are
  refused after `GH_PACED_MAX_WAIT`;
- deleting a lease file by hand frees the slot while its gh is still running;
- a nested process that closed its inherited files waits for its parent like
  any other write.

Times in messages are US Eastern by default. Set `GH_PACED_DISPLAY_TZ=UTC` to
print UTC instead.

## Configuration

Settings come from three layers, applied in order:

1. **Built-in defaults**, shown in the tables above.
2. **An optional JSON config file** at `$GH_PACED_CONFIG`, else
   `$XDG_CONFIG_HOME/gh-paced/config.json`, else
   `~/.config/gh-paced/config.json`. A missing default file is fine. A file
   named by `$GH_PACED_CONFIG` must exist. Unknown keys are an error (exit 78).
3. **Environment variables**, which can only **tighten** a limit. A looser value
   is ignored with a warning. The exceptions are `GH_PACED_MAX_WAIT`,
   `GH_PACED_GIT_MAX_WAIT` and `GH_PACED_LOCK_WAIT`, which accept any value in
   their range because they decide how long a caller is willing to wait, not
   how fast GitHub is called.

Every duration must be a finite number of seconds no longer than 7 days
(604,800 s): `cooldown_secs`, `plain_403_cooldown_secs`, `max_wait_secs`,
`git_credential_max_wait_secs`, `min_watch_interval_secs`, `GH_PACED_MAX_WAIT`
and `GH_PACED_GIT_MAX_WAIT`. `rate_limit_timeout_secs` is
at most 3,600 s. A longer value is a configuration error (exit 78), so no
deadline computed from it can overflow.

Config file keys, with defaults:

```json
{
  "read":           { "per_minute": 20, "burst": 10, "per_hour": 500 },
  "search":         { "per_minute": 5,  "burst": 2,  "per_hour": 150 },
  "write":          { "per_minute": 2,  "burst": 1,  "per_hour": 30, "max_in_flight": 1 },
  "git_credential": { "per_minute": 6,  "burst": 1,  "per_hour": 120 },
  "paginate_cost": 10,
  "watch_cost": 20,
  "min_watch_interval_secs": 30,
  "max_wait_secs": 900,
  "git_credential_max_wait_secs": 30,
  "lock_wait_secs": 30,
  "cooldown_secs": 900,
  "plain_403_cooldown_secs": 900,
  "rate_limit_refresh_secs": 300,
  "rate_limit_refresh_calls": 50,
  "rate_limit_min_refresh_secs": 60,
  "rate_limit_timeout_secs": 20,
  "halve_below_fraction": 0.5,
  "block_below_fraction": 0.2,
  "max_body_bytes": 8192,
  "max_base64_run": 1000,
  "display_tz": "US-Eastern"
}
```

The config file is checked against GitHub's documented ceilings. A READ, SEARCH
or WRITE rate above GitHub's own number (for example WRITE above 80/min or
500/hour) is clamped to it with a warning. A burst of more than ten minutes of
refill is also clamped.

The safety settings have floors that no config file can weaken. A value outside
its range is a configuration error (exit 78), not a silent clamp:

| Key | Allowed range | Default |
| --- | --- | --- |
| `cooldown_secs` | at least 900 | 900 |
| `plain_403_cooldown_secs` | at least 900 | 900 |
| `block_below_fraction` | 0.2 to 0.9, and no larger than `halve_below_fraction` | 0.2 |
| `halve_below_fraction` | 0.5 to 1.0 | 0.5 |
| `max_body_bytes` | 256 to 8,192 | 8,192 |
| `max_base64_run` | 100 to 1,000 | 1,000 |
| `rate_limit_refresh_secs` | 60 to 300 | 300 |
| `rate_limit_refresh_calls` | 1 to 50 | 50 |
| `rate_limit_min_refresh_secs` | 10 to 300 | 60 |
| `write.max_in_flight` | 1 to 4 | 1 |
| `paginate_cost`, `watch_cost` | at least 1 | 10, 20 |
| `lock_wait_secs` | 0.1 to 3,600 | 30 |

Only `GH_PACED_ALLOW_LARGE_BODY=1`, set for one call, loosens the content
guard.

Environment variables:

| Variable | Meaning |
| --- | --- |
| `GH_PACED_ACCOUNT` | default for `--account` |
| `GH_PACED_REAL_GH` | default for `--real-gh` |
| `GH_PACED_MAX_WAIT` | longest total sleep before refusing, seconds (default 900, at most 604,800); a larger value is an error (exit 78) |
| `GH_PACED_GIT_MAX_WAIT` | longest total wait for a GIT_CREDENTIAL call before refusing, seconds (default 30, at most 604,800; the smaller of it and `GH_PACED_MAX_WAIT` applies), including its waits for the state lock (exit 70 when the lock was not free in time). During a cooldown that call is refused without waiting |
| `GH_PACED_LOCK_WAIT` | longest wait for the state lock, seconds (default 30, 0.1 to 3,600); then exit 70 |
| `GH_PACED_{READ,SEARCH,WRITE,GIT}_{PER_MINUTE,BURST,PER_HOUR}` | tighten one budget |
| `GH_PACED_PAGINATE_COST` | raise the `--paginate` cost |
| `GH_PACED_DISPLAY_TZ` | `US-Eastern` (default) or `UTC` |
| `GH_PACED_ALLOW_LARGE_BODY` | `1` skips the content guard for this call |
| `GH_PACED_ALLOW_WATCH` | `1` runs watch loops at their estimated charge (refused with exit 64 otherwise) |
| `GH_PACED_ALLOW_FAST_WATCH` | `1` allows watch intervals under 30 s |
| `GH_PACED_EDITOR` | set by gh-paced for its own editor guard: the editor gh would otherwise have used. Do not set it yourself |
| `GH_PACED_STATE_DIR` | state directory (default `$XDG_STATE_HOME/gh-paced` or `~/.local/state/gh-paced`) |
| `GH_PACED_CONFIG` | config file path |

## State, the audit log, and `status`

State lives in the state directory (mode 0700, files 0600), one set of files per
account:

- `<account>.json` holds the buckets, the hourly windows, the in-flight writes,
  any cooldown, and the last `GET /rate_limit` snapshot.
- `<account>.lock` is taken with `flock` around every short read-modify-write
  step. It is never held across a sleep, a network call or a write to stderr:
  messages decided under the lock are printed after it is released, so a
  caller that has stopped reading gh-paced's stderr cannot stall every other
  process on the host. The clock is read after the lock is taken, so a process
  that waited for the lock never charges at a stale time. A process that cannot
  take the lock within `GH_PACED_LOCK_WAIT` (default 30 s) gives up with exit
  70 and a message naming the lock file. A GIT_CREDENTIAL call waits for it
  only within what is left of `GH_PACED_GIT_MAX_WAIT`.
- `<account>.audit.jsonl` is the append-only audit log, rotated to `.1` past
  10 MiB.
- `<account>.cooldown` is a copy of the latest pushback cooldown, kept apart
  from the state file. Every load takes the later of the two, so a damaged
  state file cannot shorten a long `Retry-After` pause.
- `<account>.lease-<nonce>` is one file per running write; its lock is the
  write's in-flight slot (see [Waiting and refusing](#waiting-and-refusing)).
  It is removed when the write ends, unless a process gh started still holds
  it.
- `snap-<pid>-<start ticks>-<nonce>/` holds one invocation's private copies of
  the body files it sends (directories 0700, files 0600, at most 64 MiB per
  file), plus a `.lock` file that the invocation locks and gh inherits. It is
  removed when the invocation ends, unless a process gh started still holds
  that lock. Every paced call also removes directories whose lock is free,
  whose creating process has exited, and that are at least 60 s old, so one left
  behind by a killed wrapper does not linger; any other `snap-*` directory is
  removed once it is a day old.
- `<account>.refused-edit.<nanoseconds>.md` is a copy of text the editor guard
  refused, at most its first 1 MiB (see
  [Text written in gh's editor](#text-written-in-ghs-editor)).
  gh-paced never removes these; delete them when you have recovered the text.

The state file is replaced atomically (written to a temporary file, then
renamed). A state file that cannot be parsed is kept for inspection as
`<account>.json.corrupt-<unix-seconds>` (a hard link; `-1`, `-2`, ... is
appended if that name is taken) and replaced, by one rename, with a recovery
state, so the state path never goes missing and a crash during recovery cannot
leave a fresh start. Because the lost file may have held a full hour of calls,
gh-paced does not start from empty buckets: in the recovery state **every class
is blocked for the next hour** (a fixed end time, so raising a limit afterwards
does not shorten it), a cooldown of `cooldown_secs` is in force, and every write
whose lease is still locked is restored as in flight. An unreadable
`<account>.cooldown` file is moved aside the same way and triggers the same
recovery, because the pause it held is unknown. Paced calls for that account on
that host are therefore refused or delayed for the next hour, with a message
that says which file was unusable and where it was moved. It clears itself
after an hour; do not delete state files to get around it.

Each audit line records the UTC and US Eastern time, host, PID, event
(`admit`, `throttle`, `refuse`, `pushback`, `deadline`, `exit`, `refresh`),
class, cost, command, a redacted argument summary, exit status, seconds waited,
and a short detail. In the argument summary, inline bodies are replaced by
`<N bytes>`, header values are redacted, and a `gh api` endpoint loses its host,
user name and password, query string and fragment (a query string can carry an
`access_token`). One of gh's ordinary aliases is recorded as its expansion,
with the expanded command's redactions. For a shell alias, an extension, an
unknown command, or an alias gh-paced refused to resolve, only flag names are
kept and every other argument becomes `<arg>`, and the same holds for such an
alias beneath one of gh's groups (`issue publish <arg>`), for `extension exec`
and `copilot`, and for a family whose flags gh-paced does not model; an
unrecognised flag of a known command keeps its name and loses its value
(`--token=X` becomes `--token`).
A `gh api` call carrying a flag gh-paced does not recognise is recorded as
`api <unparsed>`, because its endpoint cannot be told apart from that flag's
value. The log never contains request bodies, tokens or environment
variables. LOCAL calls are not audited, and neither is the editor guard (a
refusal there shows in the audit only as gh's non-zero exit) or pushback read
by a `gh-paced --drain` process after gh-paced has exited.

`gh-paced status [--account NAME | --all] [--json]` reads these files and prints
each class's tokens and burst, rate, use in the last hour, and time to the next
one-token slot. It also prints in-flight writes, any cooldown, the cached GitHub
numbers and their age, and the last 5 audit records. It takes no lock, creates
no files, never contacts GitHub and never changes state; it can run while every
other process is stuck.

## Exit status

| Status | Meaning |
| --- | --- |
| gh's own | the call ran; gh-paced returns gh's status, or dies by gh's signal |
| 75 | refused: the wait would exceed `GH_PACED_MAX_WAIT` (`GH_PACED_GIT_MAX_WAIT` for GIT_CREDENTIAL, which then also prints `quit=1` on stdout, as it does when the call became admissible only after that bound), a GIT_CREDENTIAL call arrived during a cooldown, the cost is above the class's burst (not for a watch) or can never fit under the hourly cap, a watch ran past the deadline its cost paid for, or gh-paced is nested too deep |
| 65 | refused by the write content guard: body too large, base64-looking content (in an argument, a file, stdin, or one of gh's alias expansions), a body gh would compose itself, or a body file that cannot be copied for inspection. Text refused by the editor guard makes the editor fail instead, so gh exits with its own status (usually 1) and sends nothing |
| 64 | usage error, or a refused command shape (a watch without `GH_PACED_ALLOW_WATCH=1`, a watch interval under 30 s, or one that is not a positive whole number, or one of gh's aliases gh-paced cannot resolve for certain; see [gh's own aliases](#ghs-own-aliases)) |
| 70 | internal error: the pacing state cannot be read or written, or its lock was not obtained within `GH_PACED_LOCK_WAIT` (within what is left of `GH_PACED_GIT_MAX_WAIT` for a GIT_CREDENTIAL call, which then also prints `quit=1` on stdout), or a GIT_CREDENTIAL call could not set the timer for its sleep |
| 78 | configuration error, `--real-gh` resolves to gh-paced, or gh's `config.yml` cannot be read and the command line names a command |
| 127 | the real gh cannot be found or run (a GIT_CREDENTIAL call whose gh never started then also prints `quit=1` on stdout) |

A caller that sees 75 should not retry immediately. The message says when the
next slot opens.

## Limitations

gh-paced is a wrapper on the client. It sees gh's command line, the files and
stdin that command line names, the text gh's editor saves, and gh's output. It
does not see the HTTP requests gh makes. Everything below follows from that,
and none of it is fixed by configuration.

- **Budgets are per host.** Hosts do not share state. The defaults divide
  GitHub's limits by four. If more hosts share an account, tighten the budgets
  with a config file.
- **Only calls through gh-paced are paced.** Calling the real gh directly, curl,
  or a library's own HTTP client bypasses it. That includes an extension that
  calls the GitHub API through a library instead of running `gh` again: the
  extension's own call is charged one WRITE token and its requests are not
  seen. A shell alias (`!...` in gh's config) is covered only where the `gh`
  it runs resolves to gh-paced.
- **Classification works from the command line.** Unknown commands, shell
  aliases and extensions are charged one WRITE token whatever they do (gh's
  ordinary aliases are expanded first and charged as their expansion). A GraphQL request whose
  query cannot be read is charged as WRITE. `gh status` is charged 10 READ
  tokens, though the number of requests it makes grows with the number of
  notifications.
- **A flag value can look like a cost flag.** `pr list --label -L50001` is
  charged 501 tokens and refused, because gh-paced does not know that
  `--label` takes the next word as its value. Write such a value with `=`
  (`--label=-L50001`). The same rule errs the other way only toward charging
  more (see [Request classes](#request-classes)).
- **Watch charges are estimates, and watches are off by default.** With
  `GH_PACED_ALLOW_WATCH=1`, gh-paced sees a watch's command line and output,
  not the HTTP requests gh makes on each poll, so it cannot count them; the
  requests per poll are worked out from what gh fetches. A `gh run watch` on a
  run with more than 100 jobs, or with several jobs that fail during the watch,
  can make more requests than it paid for before its deadline; the deadline
  still bounds how long it runs and the 30 s floor how often it polls.
- **The in-flight slot is held by a lock on a file.** Deleting a lease file by
  hand frees the slot early; see [Waiting and refusing](#waiting-and-refusing).
- **The content guard checks what it can name.** It does not see:
  - text gh composes without an editor from a source gh reads later; those
    forms are refused instead (see
    [Bodies gh composes itself](#bodies-gh-composes-itself)), including
    interactive `pr create`, `comment --editor` and `create --editor`;
  - text typed at gh's prompts rather than in the editor, such as an
    interactive `issue create` title or a `pr merge` commit subject (short
    single-line fields), or a template default submitted without opening the
    editor;
  - text written in a browser after "Continue in browser" or `--web`.
- **gh reads its configuration again.** gh-paced reads gh's `config.yml` and
  extensions directory once, before admission, and gh reads both again when
  it runs. If either changes in between (for example, something rewrites
  `~/.config/gh` while the call waits for a slot), gh can run a command other
  than the one gh-paced classified, whether the call was an expanded alias, a
  shell alias or a plain command. That is outside what gh-paced defends
  against: it paces well-meaning agents, and does not guard against a process
  changing gh's configuration underneath a waiting call.
- **The editor guard has side effects of its own.** The write slot stays taken
  while you edit, so other writes on the host wait (and are refused after
  `GH_PACED_MAX_WAIT`). Any reads gh makes between prompts are not charged
  separately. A refusal is not in the audit log; it shows there only as gh's
  non-zero exit.
- **Encoding detection works by shape.** An encoding broken up by spaces or
  punctuation, one wrapped at varying widths in lines under 4 characters, or
  one wrapped at varying widths under 20 columns whose lines lack an upper-case
  letter (lower-case base32 or hex, say; a line of one repeated character
  still counts) is not seen. A long list of equal-width tokens, or of mixed-case names, one
  per line can be refused (see [the write content guard](#the-write-content-guard)).
- **Pushback detection depends on gh's error text.** gh-paced recognises the
  phrases GitHub and gh use today. A change in that wording could hide a
  pushback, but GitHub's account-wide counters (above) still apply. With
  `--paginate`, a page body containing a CRLF header block can start an
  unneeded cooldown (see [pushback](#github-pushback-and-the-cooldown)).
- **Output written after gh exits is handed to a drainer, with limits.** The
  `gh-paced --drain` process that copies a stream still open after gh exits
  (see [What passes through unchanged](#what-passes-through-unchanged)):
  - starts a fresh pushback scanner, so a phrase split exactly across the
    handoff can be missed;
  - scans stderr only, so `--include` headers on stdout after the handoff are
    not read (gh-paced reads what can be read at once, up to 1 MiB, before it
    hands stdout over, so this concerns what a process gh left behind writes
    later);
  - records a cooldown but writes no audit record;
  - writes alongside gh-paced's own final messages, which can interleave with
    the drained output.

  If gh-paced cannot find its own executable or start the drainer, the stream
  is closed instead, later output is lost (the writer gets a write error or
  SIGPIPE), and gh-paced prints a warning saying so.
- **A signal can arrive too early or too late to end gh-paced at once.** A
  signal sent while gh runs goes to gh; if gh exits but the program reading
  the output has stopped reading, gh-paced keeps waiting until a second signal.
  A signal in the few milliseconds between gh's exit and gh-paced noticing it
  is lost. See [What passes through unchanged](#what-passes-through-unchanged).
