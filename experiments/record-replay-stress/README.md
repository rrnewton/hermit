# Record/replay multi-threaded stress

This experiment records complex concurrent workloads once, replays each
recording three times, and compares guest stdout, normalized guest stderr, and
exit status byte-for-byte.

## Result

Two of the three requested workload classes recorded successfully and replayed
identically three times.

| Workload | Record exit | Replay exits | Byte-identical | Result |
| --- | ---: | --- | --- | --- |
| Python multiprocessing | 0 | 0,0,0 | Yes | Pass |
| pthread plus pipe IPC | 0 | 0,0,0 | Yes | Pass |
| GNU Make -j4 | 124 | Not run | Unavailable | Recording timeout |

The successful output hashes are preserved in results.tsv. Both successful
workloads also had empty normalized stderr, with SHA-256
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855.

## Workloads

- python_multiprocessing uses OSS /usr/bin/python3 3.9.25 to fork four real
  multiprocessing.Process workers, print their completion records through
  inherited stdout, and join every child. The runner pins this interpreter
  because PATH selects /usr/local/bin/python3 3.12.13+meta, which timed out in
  record mode during preliminary stress runs.
- pthread_pipe releases four pthread workers through a barrier. Each worker
  sends a fixed-size completion message over a pipe; the parent prints arrival
  order and a deterministic checksum after joining all workers.
- make_parallel asks GNU Make 4.3 to run four targets concurrently with -j4.
  Each target invokes a small precompiled worker, writes a stamp, and prints its
  completion record. The fixture passes natively, including all four stamps and
  final hashes, but record mode times out before producing stdout, stderr, or a
  recording ID. Replacing in-guest GCC with the small worker produced the same
  timeout, isolating the limitation to parallel Make process/jobserver behavior
  rather than compiler complexity.

The Make timeout is a compatibility finding, not a deterministic replay claim:
without a completed recording there is nothing valid to replay.

## Method

The runner gives each record or replay 30 seconds. For a completed recording it
runs replay --autopilot three separate times against the same immutable
recording and requires:

1. every replay exit status to equal the record exit status;
2. every replay stdout stream to equal recorded stdout byte-for-byte;
3. every normalized replay stderr stream to equal recorded guest stderr.

Hermit's record-completion banner and timeout's own diagnostic are excluded
from guest-stderr comparison. Raw streams, statuses, recording metadata, and
per-thread event files remain under target/record-replay-stress/. Compact
results and environment metadata are checked in as results.tsv and
metadata.txt.

## Run

From the repository root:

    cargo build -p hermit
    ./experiments/record-replay-stress/run.sh

Override HERMIT_BIN, PYTHON_BIN, CASE_TIMEOUT_SECONDS, or ARTIFACT_ROOT when
needed. The runner continues across failures, writes diagnostics for every
case, and exits nonzero unless all workloads complete recording and all three
replays match.
