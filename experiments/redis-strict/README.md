# Redis Under Strict Hermit

This experiment builds Redis 7.2.4 and runs real `redis-server` and
`redis-cli` binaries under `hermit run --strict`. The version is pinned to
a BSD-licensed Redis release, and the source archive is verified with
SHA-256 before it is extracted.

The extended workload covers strings, counters, lists, hashes, sets, sorted
sets, Lua, streams, background persistence, restart/reload, and clean server
shutdown. It runs twice and requires byte-identical stdout and stderr. The
runner also executes Redis's built-in memory test under strict Hermit.

```bash
experiments/redis-strict/run.sh
```

The first run downloads and compiles Redis beneath `target/redis-strict`.
Set `HERMIT_BIN`, `ARTIFACT_ROOT`, or `JOBS` to override the defaults.

## Upstream Tcl Runner

Redis's upstream Tcl coordinator currently times out in this environment even
for `unit/printver`. Under strict Hermit, a syscall trace shows Tcl's background
`exec` helper blocking in `pselect6`; Hermit does not yet release the scheduler
turn around that passthrough syscall. The native control reaches the same test
but also times out waiting for its client here, so it is not a valid passing
control.

Set `REDIS_RUN_UPSTREAM_PROBE=1` to run that bounded diagnostic after the
passing source-build suite. The probe is expected to fail until `pselect6`
scheduler handling is implemented; it is kept out of CI and does not weaken
the direct Redis server coverage.
