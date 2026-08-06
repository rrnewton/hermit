# backend-parity-c measurement snapshots

Base: hermit `4c70658e785834737cbe1524f77330c781a6f5ea`, ptrace, portable lane,
debug build, devbig014. Runtime `LD_LIBRARY_PATH` pointed at the fbsource
libunwind (`third-party-buck/platform010/build/libunwind/lib`), which earlier
sweeps did not set and which is the likeliest cause of their lower pass counts.

## `compile-sweep-*.txt` — 34 COMPILE / 51 NOCOMPILE

`cc -std=c11 -O2 -g -Wall -Wextra -Werror` plus each cell's own `build.cflags`.
Needs no hermit, so it is cheap and can gate the whole family independently.
Every NOCOMPILE is a missing feature-test macro or header, not a real defect.

**Do not fix these by adding `-D_GNU_SOURCE` globally**: `numa_node_identity`
defines it itself and declares no cflags, so a global flag breaks it with a
redefinition under `-Werror`. It must be per-test.

## `verify-sweep-*.txt` — 34 PASS / 14 ERROR

Per-cell `--include-manual --test <id> --mode verify --backend ptrace`, one
distinct `E2E_RUN_ID` and one `--results` file each.

**Two traps that void a sweep, both hit by earlier attempts and by me:**

1. A shared `--results` path is **truncated per invocation**, so a loop writing
   to one file leaves only the last cell.
2. Do not read the verdict by grepping the log for the first `PASS|FAIL|ERROR`
   token — that picks up unrelated lines. I did, and it reported `34/34 PASS`
   while the authoritative JSONL said 34 PASS **and 14 ERROR**. Read `outcome`
   from the per-cell JSONL, filtered to `mode == "verify"`.

Also: do not edit the tree while a sweep runs. A prior sweep was voided from
cell 23 onward by a concurrent edit to `ci/manifest-plan`.
