# Completeness critique of REPORT.md

Not "no material gaps". The report holds up on most of what I spot-checked, but it has 7 wrong or overstated claims, 13 gaps and 5 roadmap-order problems. Repo paths are relative to `<agent-utils checkout>`; `DH` means `<consumer-a checkout>`. Consumer names are redacted as in REPORT.md.

**Confirmed as stated:**
- The `TOKIO_WORKER_THREADS` cap exists only in Python (`py/herdr_run/client.py:411`). The live `herdr-run-server` started 2026-10-03 01:56:28 PDT, has 467 threads and no `TOKIO_*` variable in its environment.
- `DH/consumer-a/agent-utils/bin -> rs/bin` (Jul 25).
- Python rejects agentcloud records (`py/agentctl/subagents.py:435-436`).
- `py/tests/conftest.py:40` imports agentctl.
- Four Rust doc files are symlinks into `py/agentctl`.
- `quickstart` and `userguide` are byte-identical across editions; the chat guides differ.

**A. Wrong or overstated claims**
1. **`send --end-of-turn` and `wait --until` are not agentctl flags.** Rust rejects both with exit 2. They are `agentcloudctl` flags that appear only in help prose (`rs/agentctl/src/cli.rs:102`, the `wait` after_help). Remove them from the "Rust only" lists in 2.1 and the summary.
2. **consumer-a's dagrun pin is misdescribed.** The real hard dependency on Python dagrun is elsewhere:
   - `DH/ci-hub/validate/start_unit.py:3013-3018` only checks that a file exists, and accepts `common/bin/dagrun` first.
   - `DH/ci-hub/bin/agent-tool:455-466` runs `exec "$PYTHON" .../py/bin/dagrun`. This is the real dependency.
   - `DH/scripts/consumer-a-box-run:121-124` and `DH/bin/consumer-a-repeat:2374-2377` use `agent-utils/common/bin/dagrun`, which resolves to Python by default.
   - So consumer-a's dagrun is Python, not "Rust in consumer-a bin". Flipping dagrun's default silently moves those two scripts. Archiving `py/dagrun` breaks agent-tool.
3. **Headline 1 is overstated.** Tracked consumer-a code reaches only herdr-run through `consumer-a/agent-utils/bin`:
   - `DH/AGENTS.md:154-162`, `DH/ci-hub/health/unpushed_parent_commits.py:105`, `DH/ci-hub/closure/ancestry_audit.py` and `verified_close.py`.
   - tick-hub and pr-landing-planner run as Python through agent-tool (`DH/ci-hub/bin/health-tick:129`, `DH/ci-hub/ci-hub.rs:2746`, `DH/ci-hub/health/pr_status.py:2035-2045`).
   - agentctl runs from pinned snapshots. No tracked code calls `agent-utils/bin/agentctl`.
4. **tick-hub's production path in consumer-a is Python.** It is agent-tool running `py/bin/tick-hub`, with a byte-identity pin on `py/bin/tick-hub` and `py/tick_hub` (`DH/ci-hub/bin/agent-tool:301-311`). Retiring Rust tick-hub is safe for this consumer; "move to Rust later" (open question 4) needs consumer changes first.
5. **pr-landing-planner has a consumer user.** consumer-a runs the Python edition through agent-tool (`pr_status.py:2035-2045`). This supports python-primary and means Python cannot be archived.
6. **The discord-gateway point is mis-framed.** `capabilities` lists it with `"origin":"reference"` and `"implemented":false` (`cli.rs:1457`). It is labelled as not implemented, not falsely advertised.
7. **"4,252 Rust vs 1,081 Python spool records" has no method or path.** The report does not say how the edition was inferred from a record, so the number cannot be reproduced.

**B. agentctl commands and flags the report misses** (from diffing every subcommand's help)

8. **Rust-only:**
   - Global options accepted on every subcommand: `--agentcloudctl-bin`, `--agentterm-bin`, `--agentcloud-url`, `--from-session` (attribution).
   - `start`: `--cloud-harness`, `--provision`, `--envspec`, `--purpose`, `--cloud-workspace`, `--node-id`.
   - `--project-box` companions: `--box-writable`, `--box-isolation`, `--box-project`, `--box-name`.
   - `read --output last` for agentcloud agents.
   - `chat tick --ready-timeout/--working-timeout/--max-attempts/--reconcile-interval`; `chat run --ignore-text-prefix/--offer-reply-command`; `chat reply --reply-id`.
   - `chat retry-checkpoint-gap` and `chat retry-boundary-gap` (with `--expected-*-sha256`, `--evidence-file`, `--keep-cursor`); `chat publish --channel-id/--request-id`.
   - All nine `inbox` subcommands: post, list, render, deliver, release, watch, park, quickstart, userguide.
   - `-V` (Python rejects it).

   **Python-only:** `chat --version/--userguide`, `chat run --interval/--observer-write-interval`, `chat context --cursor/--limit`.
9. **`--state` means different things after `chat`.**
   - Python: `chat <cmd> --state DIR` is the bridge state directory, default `<registry>/.chat` (`py/agentctl/cli.py:257`).
   - Rust: `--state` is an alias of `--registry`, and the bridge needs `--bridge-state`.
   - No step migrates Python's `.agentctl/.chat`. Add this to 3.2 and D4.

**C. Gaps in the archive checklist**

10. **Tests that break when the differentials go:**
    - `py/tests/test_cross_skip_accounting.py:1352-1361` monkeypatches `compare_pr_landing_planner`, `compare_herdr_agent` and `compare_agentctl`, and `:1370` asserts "all 7 tools agree". It breaks under step 1 of 2.4 and under 3.5.
    - `py/tests/test_validation_graph_contract.py:418` names `cross/agentctl_differential.py`.
    - 3.5 never deletes `cross/agentctl_differential.py` or `cross/herdr_agent_differential.py` themselves.
11. **The fake-Herdr fixture is Python inside the differential.** It is embedded in `cross/herdr_agent_differential.py:67,891`. E1 has to move it before 3.5 deletes that file. "Self-contained" also has a catch: Rust tests already call `/usr/bin/python3` (`rs/agentctl/src/client.rs:2885`).
12. **C1 misses files the Python edition still needs:**
    - `py/agentctl/CHAT_USER_GUIDE.md` (`scripts/embed_userguides.py:196-197`).
    - `py/agentctl/pyproject.toml:9,40` (readme and package-data).
    - Python's guide loader reads these files from the package (`py/agentctl/cli.py:29-31`).
    - While Python is the default, `py/agentctl` must keep symlinks to the moved docs, or Python `quickstart` and `userguide` break.
13. **Docs and issues to update:**
    - Add `README.md:55` ("Google Chat works today through the polling transport", which is Python-only).
    - The open P0 issue `.minibeads/issues/agent-utils-30.md` (pidfd-process-ownership) targets the "legacy agentctl backend", i.e. headless. Close or re-scope it when headless is dropped.
    - This repo tracks issues in `.minibeads/` (83 files) as well as on GitHub; D1 should say which tracker it uses.
14. **pr-landing-planner step 1 also needs:** the `rs/bin/cargo-runner:27-30` case arm, `RUST_TOOLS` in `py/tests/test_repo_dispatch_wiring.py:15-23`, and the test from item 10.

**D. Roadmap steps whose order would break users**

15. **Do not repoint consumer-a's `bin` (5.0 / 2.5) before B1.** It would send any ad hoc `consumer-a/agent-utils/bin/agentctl` or `herdr-agent` call to Python. consumer-a's registry here has 1 agentcloud record out of 7 (`DH/.agentctl`), which Python rejects. Wait for an A5 override that pins agentctl to Rust, or for B1. Repointing is fine for herdr-run.
16. **C2 cannot run "in parallel now".** Both editions print the same `USER_GUIDE.md` and `QUICKSTART.md` (verified identical), and `scripts/check_documented_defaults.py:177-200` pins Python's `cli.py` and `agent.py` defaults against that guide. A Rust-only guide before B1 would hide features from users still on the Python default.
17. **B1 through cargo-runner needs write access to the checkout.** This is inferred from the code; I did not run it in a box.
    - It creates `rs/.agent-utils-locks` and opens the lock for writing (`rs/bin/cargo-runner:105-108`).
    - It builds into `rs/target` (`:172-177`) and writes `rs/.agent-utils-snapshots` (`:229-245`).
    - It needs `git ls-files` (`common/bin/rs-source-fingerprint:26-27`).
    - It holds one global lock for all Rust tools, and runs `cargo clean` on the package after any change under `rs/` (`:150-166`).
    - wrkslots boxes are read-only outside their writable paths by default, with `$HOME` read-only (`py/wrkslots/sandbox.py:253-255`). A coordinator in a `--slot` or `--project-box` box calling `$H/agent-utils/bin/agentctl` would probably fail.
    - Make "works read-only inside a default box, without cargo" an A6 acceptance criterion.
18. **5.1 contradicts the resolver and 2.4.** "Run the only edition with a note" reverses the resolver's documented no-fallback rule (`common/bin/engine-resolver:11-13,47-49`; `setup:128-132`). It also conflicts with step 2 of 2.4, which refuses a Rust request for pr-landing-planner. State the rule once.
19. **Add to headline 7:** `scripts/validate.py:537` runs `common/bin/dagrun` through the resolver. Exporting `DAGRUN_ENGINE=rust` to try Rust agentctl also moves the whole validation graph to Rust dagrun.

**E. Tools left out of the summary table** (all single-language; list them as keep or out of scope)

20.
- `vibe-talk/`: a Rust and TypeScript service with its own Cargo workspace; grep finds no agentctl dependency.
- `scripts/agent-log-archive/`: a Python tool.
- `scripts/rebase-delta-guard`: a bash "TRACKED tool".
- `py/wrkslots.py`: a compatibility shim.
- `py/agent_team_timeline/`: an untracked leftover containing only `__pycache__`.