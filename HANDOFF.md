# Passed-count activation continuation

Assigned write destination only:
/home/newton/work/dev-hermit/worktrees/slots/mega-ci-passed-count-activation-20260915
Branch: mega-ci/passed-count-activation-20260915
CURRENT frozen head: 959ad5f180e8245e15f1588326d8a6a676264312
CURRENT base: 9ed1420f874ae341170f3edefbde35e40abe0028
CURRENT tree: ed343ac00bdb0bc30bcf15be91502383773e17d7
CURRENT diff SHA256: 41e735f1564a843ca0ab2796b97271d21b7d39b402d3b3168876ac8b52698064

This untracked handoff is explicitly coordinator-authorized. Tracked source
and index are clean; the only untracked source-tree path is this HANDOFF.md.
Keep source/head frozen while the independent review runs. No push, PR, merge,
backend guest test or full validation is authorized to this worker.

The coherent commit changes 13 files (+70/-52): the original 12-file uniform
Reverie activation plus a comment-only correction in detcore/src/lib.rs. The
pin advances 46 logical entries across ten Cargo files and three DBT bindings
from 8c8c0a57649c9ffbf8a7a14291a64320f64b935f to
e68eed20e9d9a7bb791684a3576871b8cbc11853. It is the minimum main-history
revision containing fe3db5af, 08a66b46, and e68eed20 (original-byte JSON parsing
required to refuse duplicate-field laundering). Producer:
https://github.com/rrnewton/reverie/pull/553.

The complete 41-commit imported range includes KVM/shared APIs/LiteInst/DBT
runtime changes. That range is unchanged by rebase. Numeric budgets and all
gates are unchanged. Every non-revision Cargo byte and all non-comment CI
script behavior were checked against the current base. Both DBT carry comments
now record ALL native recipe inputs: build.rs object
0ff8ae24b97464044735ba79ea74765ba4ac3ff0, vendor/dynamorio
a3c41e5d3630c06ba9ff94d5a779792273268df5, and third-party/
fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a, identical at both pins. The
Detcore comment now correctly says faccessat2 remains untyped without including
fchmodat2. No dispatch logic changes. Commit body uses exact who-am-i output
[hermit2, degraded-unresolved, gpt-6-astra, devbig014, role=impl]
and includes Relationship to gVisor.

Current evidence: target/passed-count-activation/refresh/
- frozen.json/frozen.diff: current identity and complete diff.
- previous-frozen.json/.diff/.commit and previous-HANDOFF.md: old evidence.
- focused-checks.json and named logs: all PASS: canonical online pin gate
  (2.888s), nine DBT wrapper controls (7.09s), root and isolated LiteInst locked
  offline metadata (17 and 9 Reverie packages), shell syntax/ShellCheck,
  cargo fmt --all -- --check, diff check, and normal commit hooks.
- source-preservation.json, who-am-i.stdout/.stderr, commit.log.

PASSED current-head compile + Clippy:
Dagrun PID 1958330, execution session 46708 completed with actual exit 0.
Cargo check exited 0 in 10.603s, Clippy exited 0 in 22.905s. Combined Dagrun
step: 33.916s wall, 100.329s CPU, cgroup peak 2,582,208,512 bytes, no OOM or
timeout, four inner jobs. Commands are cargo check --locked
--workspace --all-targets --all-features and cargo clippy with the same flags
plus -- -D warnings. Pinned agent-utils 14589 Dagrun, 4 CPUs / 16 GiB /
1800s wall / 7200s CPU; target cache reused from the earlier compile.
Artifacts refresh/compile/{invocation.json,dagrun-invocation.json,dagrun.log,
cargo-check.log,clippy.log,checks-status.json,dagrun-status.json,perf/}.
Actual exit was captured before output inspection. Not a benchmark.

PASSED independent Claude delta review:
PID 2046690, execution session 53466 completed actual exit 0, no timeout,
178.665s, 11 resumed turns. Dedicated Claude UUID
066b64a3-0110-439b-834c-f3a9b328efef. Actual JSON modelUsage identifies
claude-opus-5[1m], canonical claude-opus-5, provider Vertex. Same installed
model/read-only plan mode/tools, no bypass. Exact CURRENT head/tree APPROVE,
no findings; all three prior lows discharged, all four goalpost categories
clean. Reviewer reproduced diff digest independently and inspected current-head
incremental check/Clippy evidence. No runtime/full receipt assurance.
Artifacts target/passed-count-activation/claude-review/delta-{prompt.txt,
invocation.json,pid.txt,stdout.json,stderr.log,status.json,review.md}.

Prior frozen head a714a2b19b513a4ebc8ac6d81ef8a895a1534a5c on base7d55fcd64
passed bounded compilation (81.118 wall /313.399 CPU seconds, cgroup peak
2,765,602,816 bytes, zero OOM) and independent Claude source review. Prior
model was claude-opus-5[1m], actual exit0; first attempt timed out actively,
resumption finished. Old certificate is claude-review/resume-review.md and
full output/status are resume-stdout.json/resume-status.json. This is historic
assurance, not evidence for the refreshed exact head.

Next: root independently inspects this frozen source before any landing.
No pin task source work remains assigned to this worker. No push/PR/full run
is authorized. Backend guest tests and a qualifying full receipt remain missing.
Current read-only next task is flip-verify-to-two-harness-managed-runs-with-retained-logs,
per explicit owner priority via root. Audit current-main implementation and
recovered S7/prerequisites, no implementation/new slot/runtime tests until an
exact gap and separate write destination are assigned. Relevant S7 final record
is /home/newton/work/dev-hermit/hermit/ignored/rescue/s7-round3-20260908/FINAL-EVIDENCE.md
(the hermit/ prefix matters). Current main 9ed1420f still uses run_cell and
internal --verify-json; cutover is not active. The separate manifest diagnosis
is queued, and its newly provisioned slot is NOT yet a write assignment here.
