# gh-paced: related work

This comparison is about **client-side pacing of GitHub API traffic**: slowing
requests down before GitHub has to refuse them, and backing off when it does.
It covers public open-source tools only, as of 2026-10.

## Existing tools

| Project | Relevant capability | Relationship to gh-paced |
| --- | --- | --- |
| [GitHub CLI](https://github.com/cli/cli) | `gh api --cache <duration>` caches responses so a repeated read is served locally. gh reports GitHub's rate-limit errors on stderr. | The program gh-paced wraps. gh has no option to pace calls or to share a budget between processes, so a loop of `gh` invocations runs as fast as GitHub answers. `--cache` is still worth using under gh-paced: a cache hit makes no request, but gh-paced cannot see that it was a hit and charges it anyway. |
| [octokit/plugin-throttling.js](https://github.com/octokit/plugin-throttling.js) | Throttles requests made through the Octokit JavaScript client, following GitHub's published best practices. It queues writes separately from reads, and calls `onRateLimit` and `onSecondaryRateLimit` hooks that decide whether to retry. With Bottleneck's Redis mode it can share limits across processes. | The closest design. It works inside one client library; gh-paced works on whole `gh` invocations from any caller. gh-paced borrows the separate write queue and the retry-after handling. It never retries on its own: after a refusal it pauses all calls and tells the caller. |
| [Bottleneck](https://github.com/SGrondin/bottleneck) | A general rate limiter and scheduler for Node.js: reservoirs, minimum spacing, concurrency limits, and a Redis-backed cluster mode. | The limiter plugin-throttling builds on. gh-paced needs the same primitives (spacing, burst, a concurrency cap) but shares state through a locked file on each host instead of a Redis server, because the hosts it targets share no service. |
| [PyGithub](https://github.com/PyGithub/PyGithub) | A Python client with built-in throttling (`seconds_between_requests`, `seconds_between_writes`) and a retry policy that honours GitHub's retry-after and secondary-limit responses. | Shows spacing writes more widely than reads inside a library. Its throttling is per client object: two processes each keep their own spacing, which is the multi-process gap gh-paced closes for `gh`. |
| [google/go-github](https://github.com/google/go-github) | A Go client that reports primary and secondary limit responses as typed errors, including the retry-after duration. It does not pace requests itself. | Leaves pacing to the caller. That is the situation gh-paced addresses for shell scripts and agents that call `gh`. |
| [gofri/go-github-ratelimit](https://github.com/gofri/go-github-ratelimit) | An HTTP transport for Go that sleeps when GitHub reports a secondary rate limit, then lets requests continue. | Reacts after GitHub pushes back, inside one process. gh-paced also paces before any pushback, and its cooldown covers every process on the host. |

These tools work well inside the programs that use them. The comparison does not
claim that token buckets, retry-after handling or file locks are new, or that
the list covers every implementation.

## Lessons used in gh-paced

**Pace before GitHub refuses, not only after.** A client that only reacts to 403
and 429 responses has already been refused once, and GitHub warns that
"continuing to make requests while you are rate limited may result in the
banning of your integration." gh-paced charges every call against a budget
first. The pushback cooldown is a second line of defence.

**Treat writes separately and strictly.** Several of the clients above separate
writes from reads. gh-paced gives writes their own bucket (one per 30 s), an
hourly cap, and one write in flight at a time per host.

**Share the budget across processes.** A per-object or per-process limiter does
not help when many short-lived `gh` processes run at once. gh-paced keeps each
account's budget in one state file per host, updated under `flock`. Hosts that
share an account each take a fixed share of GitHub's documented limits, and the
account-wide `GET /rate_limit` numbers cover what local accounting cannot see.

**Unknown means expensive.** A wrapper sees command lines, not HTTP requests.
Extensions, shell aliases and new `gh` subcommands are therefore charged as
writes until they are classified explicitly; gh's ordinary aliases are expanded
by the wrapper and charged as what they expand to.

**A refusal says what to do next.** Every refusal names the budget, the time of
the next slot, and the exit status, so the caller can decide whether to wait or
give up without guessing.
