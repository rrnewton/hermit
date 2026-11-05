> Working audit, 2026-10-07, by an agent ([opus 5.5]). Consumer project names and hosts are
> redacted as `consumer-a`, `host-a`, and `host-b`. Section numbers refer to this file; the
> corrections in CRITIQUE.md override the report where they disagree.

# Edition audit: Python and Rust editions in agent-utils

Snapshot: HEAD `d452a8b7` (2026-10-07), checked from host-a. The audit was read-only. Each tool was audited once and then checked by an adversarial verifier. This report uses only gaps the verifier confirmed or partly confirmed, with its corrections applied. Effort estimates come from the audits: S is about a day or less, M a few days, L one to two weeks, XL more. Line numbers in `rs/agentctl/src/cli.rs` are drifting by about 12 lines because another agent is editing that file today.

## Headline findings

1. **The "Python by default" rule does not hold in production.**
   - consumer-a's submodule has an untracked `consumer-a/agent-utils/bin -> rs/bin` symlink, dated Jul 25. I checked it today; the submodule is at `adf8301c`.
   - That symlink bypasses `common/bin/engine-resolver`, so every paired tool consumer-a calls through that `bin` runs the Rust edition through cargo-runner. That covers agentctl, herdr-agent, herdr-run, dagrun, cpuset-alloc, tick-hub and pr-landing-planner.
   - consumer-a's own instructions send agents to that path for herdr-run (consumer-a `AGENTS.md:154-162`).
   - Across every herdr-run spool on this host there are 4,252 Rust-written run records against 1,081 Python ones.
2. **A Python-only safety fix is missing from the live server.**
   - Python herdr-run starts the Herdr server with `TOKIO_WORKER_THREADS=16` (`py/herdr_run/client.py:53-59,410-411`). Rust does not (`rs/herdr-run/src/client.rs:430-442`).
   - The `herdr-run-server` running now on this 316-core host was started by a Rust call at 2026-10-03 08:56:28Z. It has no worker cap and 463 threads.
3. **The repository-default Python agentctl already breaks on a real registry.**
   - It rejects agentcloud records (`py/agentctl/subagents.py:435-436`), which only Rust can create.
   - As a result, `agentctl list` on consumer-a's registry exits 1.
   - Separately, the shipped wrkslots guide tells users to run `agentctl start --project-box`, which exits 2 under the default engine.
4. **A version string does not identify a build.**
   - Four different agentctl builds on this host print `agentctl 0.1.0`. One of them, the Sep 27 build at `~/.local/bin/agentctl`, lacks `inbox` and `chat thread`.
   - `tick-hub 0.2.0` names two different feature sets.
5. **Machine outputs are already byte-identical; the drift is around them.** dagrun, tick-hub and pr-landing-planner produce byte-identical machine output. The differences are in help text, stderr, flags that exist in only one edition, and runtime features. The differentials cannot see these because they check lists of required flags, not equality.
6. **Moving `py/agentctl` today breaks three things:**
   - the Rust build, because four Rust doc files are symlinks into `py/agentctl` and three are compiled in with `include_str!`;
   - every test in `py/tests`, because `conftest.py:40` imports agentctl;
   - `make check`.
7. **One global switch controls every paired tool.** `DAGRUN_ENGINE` (`common/bin/engine-resolver:34`) flips all paired tools at once. With it set to `rust`, every Python-only command exits 127.

## 1. Summary table

| Tool | Editions | Non-test LOC py / rs | Commits in 30 days py / rs | Default: repo / observed in use | Recommendation | Why |
|---|---|---|---|---|---|---|
| agentctl (with herdr-agent) | both | 26.8k / 40.8k | 54 / 98 | Python / Rust in every deployment seen | **rust-primary-archive-python** | All live use and all new features are Rust; no user found for the Python-only extensions |
| herdr-chat | Python only | (in agentctl) | n/a | Python | **retire with the agentctl archive** | Rust `chat` has different flags and state; an alias would mislead |
| herdr-subagents | Python only | ~1.75k (plus ~5.1k shared headless runtime) | 4 / 0 | Python | **retire with headless; keep in archive** | No state directory and no references anywhere |
| dagrun | both | 29.4k / 36.8k | 30 (20 source) / 35 | Python / Rust in consumer-a `bin` | **rust-primary-archive-python** | Large and fast-moving; Python already follows Rust. Gated by Rust distribution and parallel-experiment-runner |
| cpuset-alloc | both (inside the dagrun package) | 1.0k / 1.4k | 1 / 1 | same as dagrun | **follow dagrun (Rust)** | Ships in the dagrun package |
| herdr-run | both | 6.2k / 7.6k | 10 / 7 | Python / **Rust** for the busiest consumer | **single edition; which one is your call** (lean Rust if a prebuilt Rust distribution is adopted) | Drift sits in the live-Herdr path, which no differential can reach |
| tick-hub | both | 3.1k / 3.5k | 1 / 1 | Python | **enforce-identical** (alternative: retire Rust, S) | Small and stable, core output byte-identical, bounded list of fixes |
| pr-landing-planner | both | 5.9k / 6.4k | 2 / 1 | Python | **python-primary-archive-rust** | Labelled experimental, no in-tree users, Rust can silently produce a different plan |
| wrkslots (plus rs/wrkslotsd) | Python, plus a Rust observer | 57.8k / 13.3k | 240 / 4 | Python | **single-language-keep; archive wrkslotsd or label it experimental** | wrkslotsd is a dormant second reader of the event log |
| wrkviz | Python | 54.5k / none | 3 / 0 | Python | **single-language-keep** | Stable, no dependencies |
| parallel-experiment-runner | Python | 2.5k / none | 2 / 0 | Python | **single-language-keep** | Stable, but it blocks archiving dagrun's Python edition |
| gh-paced | Rust | none / 16.2k incl. tests | 0 / 26 | installed by hand | **single-language-keep** | PTY, signal and startup-cost requirements |
| chat-subscription crates | Rust | none / 8.2k | 0 / 12 | library | **single-language-keep** | Only Rust agentctl uses them |

## 2. Per tool

### 2.1 agentctl

The plan is in section 3. In brief:
- **Shared and at parity:** the interactive Herdr lifecycle, profiles, skill install, the flat registry format, and quickstart and userguide (byte-identical, via symlinks).
- **Rust only:** agentcloud, `inbox`, `--project-box`, the plugin chat host, `send --end-of-turn`, `wait --until`, `stop --skip-cloud-halt`, and stronger delivery evidence.
- **Python only:** headless workers, reset/repair/migrate, `mcp`, the polling Google Chat transport, herdr-subagents, herdr-chat, and Muse 1.4 composer recognition.
- **Differential coverage:** `cross/agentctl_differential.py` covers the interactive core and a real Python-to-Rust handoff on one registry. It covers nothing that exists in only one edition, and it refuses to compare `agentctl chat` (`cross/README.md:89-91`).

### 2.2 dagrun and cpuset-alloc

**Byte-identical (checked by hand on both editions):**
- `list`, `ascii`, `dot` and `json`, including the `--labels` and `--group-by` views;
- `plan` for greedy-lpt, critical-path and cpa, with and without profile feedback;
- `summary build`, `summary stats` and `summary plan`;
- `capabilities` and `--version`;
- loader errors.

**Rust only:**
- **Structured test results at run time.**
  - Rust runs and enforces them: `rs/dagrun/src/scheduler.rs:3614-3617` and `test_results.rs`.
  - Python refuses such a graph with exit 1 (`py/dagrun/scheduler.py:2447-2459`), and its docs never say so.
  - Correction from verification: `StructuredTestResultsManifest` exists in both editions (`py/dagrun/model.py:239-266`). Only the producer types (`TestResult`, `TestResults`, `write_*`) are Rust-only.
- **`run --resource-caps-path` and `DAGRUN_RESOURCE_CAPS_PATH`** (`rs/dagrun/src/resource_caps.rs`). Python rejects the flag and silently ignores the variable.
- **`run --no-color` and the `DAGRUN STEP ERROR [tag]` marker** (`rs/dagrun/src/scheduler.rs:1510,4450-4451`).
- **`DAGRUN_STEP_STARTED_MONOTONIC_NS`** (`scheduler.rs:393-399,3630`). Nothing in the repo reads it.

**Python only:**
- **`DAGRUN_COPY` and `DAGRUN_COPIES` for `--stress` copies** (`py/dagrun/cli.py:1749-1793`).
  - Rust's `expand_stress` (`rs/dagrun/src/cli.rs:2649-2671`) does not set them, yet the Rust `--userguide` promises them (`common/docs/dagrun/USER_GUIDE.template.md:695-716`).
  - Under Rust, all stress copies then write one file.
- **Library-only items:**
  - `dagrun.envcheck`, which has no in-repo importer but is a documented public module;
  - the dead `DagConfig.known_failures` field (`py/dagrun/model.py:1002-1009`).

**Behaviour differences:**
- **Bundled short flags.** `run -vv`, `-kv`, `-qv` and `-qq` are rejected by Rust (`rs/dagrun/src/cli.rs:1446`), although Rust `sweep` accepts `-vv`.
- **Extra arguments ignored.** `quickstart --help`, `capabilities extra` and `--userguide extra` exit 0 on Rust and 2 on Python (`cli.rs:5177-5193`).
- **Empty `NO_COLOR`.** Python keeps colour (`py/dagrun/cli.py:211`); Rust turns it off.
- **`dagrun yaml` text.** Python keeps field order and wraps lines; Rust sorts keys. Correction from verification: this output is not compared at all, not even structurally (`cross/differential.py:1356-1370` runs `json`).
- **Help and error text.**
  - Rust help is much terser: `run --help` is 209 lines in Python and 39 in Rust.
  - Error prefixes differ: `dagrun:` in Python, `dagrun run:` in Rust.
  - Rust omits `[--userguide]` from its invalid-command usage line (`cli.rs:5363-5364`).
- **cpuset-alloc:**
  - Rust subcommand help is a bare usage line (`rs/dagrun/src/cpuset_allocator.rs:28-38`).
  - Rust prints a malformed `cpuset-alloc: : unknown command` (`:629-631,678`).
  - Neither edition has quickstart or a user guide.

**Differential coverage:**
- About 50 compare functions.
- `compare_cli_schema` (`cross/differential.py:6884-6935`) checks only that a list of required flags is present, although its docstring claims a complete inventory. The module docstring (`:76-77`) claims byte-identical `--userguide` output, but only `version` and `capabilities` are compared byte for byte (`:6865`).
- `run` stdout is compared in one `--show-plan` case only.
- Live cpuset-alloc parity is skipped in validation (`validation/cross.dag.yaml:43,79`). It runs only under a manual `make cross`, which no workflow runs (issue #165; the repo records no slug for it).

**Recommendation: rust-primary-archive-python.** Python is already a follower: in 30 days, only one py/dagrun commit had no Rust counterpart, and that was a same-day port (`fafe61df` of `3f682c95`). Startup cost is not an argument for Python. Through cargo-runner Rust takes about 0.32 s against Python's 0.17 s, but the raw release binary starts in about 0 ms.

Preconditions:
1. **Port to Rust:** `DAGRUN_COPY`/`DAGRUN_COPIES` (S), bundled short flags (S), and argument rejection for `quickstart`, `capabilities` and `--userguide` (S).
2. **Ship a Rust distribution** that does not run cargo on every call (section 5.2).
3. **Re-home parallel-experiment-runner.** This is the critical path. It drives Python dagrun in-process: `cg.Cgroups`, `install_scope_teardown`, `reexec_in_scope`, `run_dag_limited`, and `StepOutcome.pids_events`, which is deliberately not written to the profile CSV (`py/parallel_experiment_runner/execute.py:117-163,240-277,372`). Neither edition's `run` has a machine-readable result output, so "switch it to the CLI" is not possible yet. Options:
   - add `run --results-json` to Rust (M), then move the runner onto the CLI;
   - port the runner to Rust;
   - vendor Python dagrun into it, which recreates two editions.
4. **Move the other Python-library users off py/dagrun:** `scripts/irq_survey.py:49`, `scripts/dagrun_profile_report.py:14`, `scripts/check_dagrun_examples.py:31` and `cross/differential.py:243`.
5. **Soak the validation graph on Rust.** `scripts/validate.py:537-549` runs the whole validation graph through Python dagrun.
6. **Coordinate with consumer-a,** which hard-codes `agent-utils/py/bin/dagrun` (consumer-a `ci-hub/validate/start_unit.py:3016-3017`).
7. **Confirm `envcheck` and `known_failures` have no external callers.**
8. **Run `make cross` once** to record a live-scope parity baseline before the freeze.

Docs to fix whichever edition survives:
- add `RELATED_WORK.md`, moving and expanding the section at `common/docs/dagrun/PLANNER_DESIGN.md:111`;
- list the edition extensions in `README.md:79-80`;
- correct the stale `result_manifests` comments (`py/dagrun/io.py:939-944`, `rs/dagrun/src/io.rs:178-182`).

### 2.3 tick-hub

The full difference list and enforcement plan are in section 4.1. Summary:
- **Identical:** stdout and the fired-state file bytes match.
- **Python only:** `tick --dry-run` and gate cleanup on signals.
- **Rust bug:** with fd 1 closed, Rust exits 0 and still persists state, consuming the reminder's cadence for a report nobody received.
- **Text drift:** help, quickstart, stderr and `yaml` output differ.
- **Coupling to the agentctl decision:** the only production path, `skills/agent-utils-setup/templates/tick-hub/tick-deliver:13,20,22,35`, calls both `$AU/agentctl` and `$AU/tick-hub` through the same resolver. Moving agentctl to Rust by exporting `DAGRUN_ENGINE=rust` silently moves tick-hub too.

### 2.4 pr-landing-planner

**Identical:** `plan`, `graph`, `clusters` and `status` output in every format, on both bundled fixtures.

**Rust can silently produce a different plan:**
- **Option values that start with `-` are swallowed.** With `plan --fixture F --gate-check --batch`, Rust takes `--batch` as the gate name, changes #942 and #1050 to `[real]`, and exits 0 (`rs/pr-landing-planner/src/cli.rs:171-188`). Python exits 2.
- **The regex dialects differ in both directions, sometimes silently.**
  - `^wasm-[[:alpha:]]+$` is accepted by both editions but yields different actions for #1049: hold-fix in Python, refire-ci in Rust.
  - Look-around and `\Z` work only in Python; `\p{L}` works only in Rust.
  - Code: `rs/pr-landing-planner/src/classify.rs:33-35,52-55`, `priority.rs:45`, versus `py/pr_landing_planner/classify.py:72,83-85`.

**Other differences:**
- **Flat parser.** Rust uses one flat parser: any flag is accepted on any command, the command may follow flags, and `-h`, `--version` and `--userguide` work anywhere (`cli.rs:190-260`). `graph --batch`, `plan --emit-demo` and `quickstart --repo x` exit 0 on Rust and 2 on Python.
- **Empty values.** Rust rejects `--net-wrapper ""`, which Python's help documents as meaning "none" (`py/pr_landing_planner/cli.py:409-413`).
- **Rust help.** Every `<cmd> --help` prints the same global help. It omits `--remote` (parsed at `cli.rs:222`), describes `--warn-threshold` wrongly (`emit.rs:636` compares it with the open-PR count), and has no examples.
- **Archive directory.** With an empty `XDG_STATE_HOME`, Rust archives to a relative path (`cli.rs:416-425`).
- **Broken pipe.** On EPIPE, Rust archives first and then panics with exit 101. Python prints first and exits 1.
- **Cosmetic:**
  - the demo fixture title differs (`py cli.py:251` vs `rs cli.rs:52`);
  - only Python uses colour;
  - Python's quickstart prints a literal `{{human,json,actions}}` (`cli.py:206`).
- **Bad example file.** `py/pr_landing_planner/examples/pr-landing-demo.yaml:4` claims "#987 rebase-then-land", but both editions now emit land-now. The same file ships internal `ds-` incident ids at lines 30, 37, 51 and 58 as package data, which `scripts/embed_userguides.py:104` forbids.

**Differential coverage.** About 2,800 lines, including a third implementation of the review-evidence digest (`cross/differential.py:9816-10006`). It does not cover per-subcommand flag scoping, empty values, regex dialect, quickstart, archiving, or the live GitHub host.

**Recommendation: python-primary-archive-rust.** Preconditions:
- move the differential's safety and review-evidence assertions into Python golden tests first;
- check whether the crate is published (crates.io returned a proxy 403).

Steps:
1. Move `rs/pr-landing-planner` to `archived/` and remove it from:
   - the `rs/Cargo.toml` members, `rs/bin`, and `setup:71,89,101`;
   - `validation/components.json` and the Rust DAG fragments;
   - `scripts/check_rust_packages.py` and the `scripts/embed_userguides.py` TOOLS list;
   - `DIFFERENTIAL_TOOLS` and `compare_pr_landing_planner`.
2. Make the resolver refuse a Rust request for this tool with a "Python-only" message.
3. Fix the Python quickstart braces and the example file, document the regex dialect (Python `re`), and add `RELATED_WORK.md`.

### 2.5 herdr-run

**Identical:**
- all 11 help screens (byte-identical), version 0.2.0, quickstart, the config template, and the user guide apart from its 2-line Installation section;
- the 20 config keys and their defaults;
- dry-run JSON and audit lines.

Both editions use the same lock paths and the same session-cache format, so mixing editions on one host is safe.

**Differences:**
- **Python-only `TOKIO_WORKER_THREADS=16`.** This is live in production; see headline 2.
- **Rust is laxer about YAML.**
  - Rust accepts non-string mapping keys (`deny_global: {1: []}`) and `!!binary`, `!!timestamp` and `!!python/str` tags (`rs/herdr-run/src/config.rs:243-249,277`). Python exits 78 for these (`py/herdr_run/yamlcore.py:97-100`).
  - The real problem is that one edition accepts a config the other refuses. Per the verifier, Rust keeps a key's spelling as text, so a key spelled `true` can still match the real program `/usr/bin/true`.
- **Python is laxer about timeouts.** It accepts non-ASCII digits in `--timeout` and `--wait-ready` (`py/herdr_run/cli.py:244-261`).
- **Readiness verdicts can differ, not just their text.** Python uses `splitlines()` and Rust uses `lines()`. With a CRLF `.bashrc`, Rust infers no prompt tail and abstains (`rs/herdr-run/src/readiness.rs:128-131,215-218,333-334`).
- **Diagnostic quoting differs** (Python repr against Rust Debug). It shows up in readiness reasons, the spool warning, `check` refusals and config errors (14 malformed-config cases checked, all exit 78). Rust prints raw U+202E and U+00A0 in refusals (`rs/herdr-run/src/allowlist.rs:324-354`).
- **systemd-run is resolved eagerly in Rust** (`client.rs:339-347`). Without systemd-run, Rust exits 69; the guide already treats that host as unsupported.
- **Absolute `--cwd` is normalized lexically in Rust.** This matters only when the lexical target does not exist, or under `set -P` or zsh's chase options.
- **JSON key order** depends on build features in Rust. This is latent: `tick-hub` enables serde_json's `preserve_order`, which leaks in under a workspace-wide build.
- **net-doctor:** Python's direct probe does not expand `~` in `probe_remote` (`py cli.py:1011-1016`), which is a small Python bug.
- **Both editions** leave the `--timeout` (900 s) and `--wait-ready` (0 s) defaults out of help, and only `run` has examples. Both still carry dead agent-control client code (`py/herdr_run/client.py:529-569,604-669,749-756`; `rs/herdr-run/src/client.rs:573-650,693-783,850-855`).

**Differential coverage.** `cross/herdr_differential.py` runs about 170 checks. It cannot reach a live Herdr: production ignores PATH and offers no way to inject a fake. The server launch arguments, readiness strings and session-cache behaviour are therefore unchecked, which is exactly where the TOKIO gap hid for two months.

**Usage.** The audit's "1077 of 1077 records are Python" holds only for this repo's own spool. Across all spools on the host it is 4,252 Rust against 1,081 Python, driven by consumer-a's stale symlink.

**Recommendation: one edition.** Do this now regardless of the choice:
- port the worker cap to `rs/herdr-run/src/client.rs:430-442` (S), or repoint consumer-a's `bin` to `common/bin`;
- restart `herdr-run-server` through a capped edition.

Then choose:
- **Rust:** what the heaviest consumer actually runs, self-contained, and consistent with agentctl and dagrun. Needs the cap, strict YAML keys and tags, lazy systemd-run, explicit key ordering, and a prebuilt distribution.
- **Python:** the documented default, more tests (304 against 195), and already capped. Needs Python 3.10+ with PyYAML in every sandbox. consumer-a must be migrated first, because removing `rs/bin/herdr-run` breaks its documented command at the next submodule bump.

### 2.6 Single-language tools

- **wrkslots.** Keep it in Python. `rs/wrkslotsd` is not a port; it is a second implementation of the event-log reader.
  - It is `publish=false`, nothing dispatches it, and the copy in `rs/target` (Sep 24) lacks the `explain` and `plan` commands now in the source.
  - consumer-a's observer timer has been inactive since 09-26.
  - It is guarded only by a static event-kind contract test (`py/tests/test_wrkslotsd_source_contract.py`), with no behavioural differential.
  - Archive it, or label it experimental and list it in `README.md`.
- **wrkslots depends on Rust agentctl in two ways:**
  - Its guide and help tell users to run `agentctl start --project-box`, which is Rust-only (`common/docs/wrkslots/USER_GUIDE.md:1730`, `py/wrkslots/sandbox.py:258`).
  - It parses agentctl's registry using hand-written fixtures (`py/wrkslots/examples/agentctl_liveness_probe.py:8-21`).
- **Rust agentctl and the global switch.** Rust agentctl finds `wrkslots` on PATH and passes its own environment through (`rs/agentctl/src/subagents.rs:2052-2063,2106-2133`). With `DAGRUN_ENGINE=rust` exported, `bin/wrkslots` exits 127. The workaround is `AGENTCTL_WRKSLOTS_BIN`.
- **wrkviz.** Keep it in Python.
- **parallel-experiment-runner.** Keep it in Python. It needs `RELATED_WORK.md`, and it blocks archiving dagrun's Python edition (section 2.2).
- **gh-paced.** Keep it in Rust. Gaps:
  - there is no `skills/gh-paced` (the setup skill covers installing it);
  - `embed_userguides.py` does not check its doc links;
  - it is not a validation component (the workaround is at `scripts/validate.py:186-191`);
  - it is installed by hand (`common/docs/gh-paced/QUICKSTART.md:38-39`). The copy installed here is current.
- **chat-subscription crates.** Keep them in Rust. Note that Rust `capabilities` lists a `discord-gateway` backend, but no Discord code exists (`rs/agentctl/src/cli.rs:1456-1461`).

## 3. agentctl: roadmap to a Rust-only edition

### 3.1 Decision for every Python-only capability

| # | Capability | Evidence | Use found | Decision | Effort |
|---|---|---|---|---|---|
| 1 | `list` keeps going past a bad row (marks it `record_error`, exits 1) | `py/agentctl/subagents.py:2311-2329`; Rust aborts with exit 75 at `rs/agentctl/src/subagents.rs:4125-4128` | needed for agentcloud, corrupt and turn-runner rows | **Port (mandatory)** | S |
| 2 | Muse 1.4 composer and status recognition (YOLO and standard footers, versioned header, staged, Goal (paused)) | `py/agentctl/client.py:106-330`, `subagents.py:885-909,987-992`, tests `py/tests/test_agentctl_client_launch.py:236-288`; Rust `client.rs:719-759`, `subagents.rs:1468-1472` | any Muse 1.4 user: Rust cannot start a YOLO-mode session and reports an idle one as working | **Port (mandatory before switching the default)** | M |
| 3 | Nested `agentctl-session/v2` and `v3` records | `py subagents.py:67-72,289-365,511-718`; Rust `schema: u32` at `subagents.rs:1080,1173` | none. The only writer was unmerged (`origin/rescue/host-b/codex-agentctl-liveness-*`, commits `fb8da813`..`2e4bb64d`), and no local registry has such rows | **Do not port.** Row 1 plus a clear refusal message covers it; port the full decoder (M) only if such a registry turns up | S |
| 4 | Headless turn-runner workers (`start --mode headless`, `--backend tmux`, `send --model`, `read --output last\|since_turn`, `--since-turn`, headless wait/attach/pause/resume/stop) | `sessions.py:71-379`, `worker_rpc.py`, `foreign/` (6,345 lines) | 0 of 239 records on host-a | **Drop; keep in archive** | port would be XL |
| 5 | Retiring existing turn-runner records | Rust refuses every non-Herdr record before looking at options (`subagents.rs:5740-5748,1243-1259`) | none locally | **Port** a guarded `forget` / stop-and-archive with `--expected-token` | S |
| 6 | `reset`, `repair`, `migrate` | `cli.py:181-197,404-407` | headless only | **Drop** | (L) |
| 7 | agy harness | `foreign/lib.py:62,363`, `agent_runner.py:244-585`; Rust still accepts agy in headless profiles (`profiles.rs:472,491`) | none | **Drop**; refuse it at profile load in Rust | S |
| 8 | herdr-subagents: 12 subcommands, its own MCP server plus a WebSocket on 127.0.0.1:18765, and the `HERDR_SUBAGENTS_POLICY` hook | `foreign/cli.py`, `foreign/mcp/server.py:24-27,466-470`, `foreign/policy.py:54-57` | `~/.local/state/herdr-agent/foreign` absent; 0 references in consumer-a | **Drop with headless**; replace `bin/herdr-subagents` with a deprecation stub | S |
| 9 | `agentctl mcp` (18 `agent_*` tools) | `py/agentctl/mcp.py:22-52` | no registration in `~/.claude.json` or `~/.codex/config.toml` | **Drop by default.** A Rust port would be a redesigned 15-tool surface, because 3 tools and several arguments are headless-only | M if wanted |
| 10 | Built-in Google Chat REST polling transport (token from `HERDR_CHAT_TOKEN` or `token_command`, `reaction_user`) | `chat.py:1470-1605`; Rust ships no provider (`cli.rs:1443-1455`, `plugins.discovered=[]`) | no live Python chat user; the setup skill sends new users to it | **Your decision (open question 1).** To make Rust self-contained: an in-repo Rust polling plugin plus a native outbound helper | L |
| 11 | `chat launch` | `chat.py:4601-4636,5139` | setup skill | **Replace** with documented `start`/`adopt`, then `chat init`, then `chat run` (S), or add a Rust `chat launch` (M). Note that `launch` leaves the coordinator unregistered (`chat.py:4627-4629`), which contradicts the setup skill (`SKILL.md:370-375`), so the Rust flow also fixes a bug | S or M |
| 12 | `chat context` (reads the provider's thread) | `chat.py:5452-5466` | none found | **Your decision:** a provider-history plugin action (M), or accept `chat thread` (retained state only) | M or 0 |
| 13 | `chat history` | `chat.py:5449-5450` | none found | **Drop**; `chat thread --last N` covers it | 0 |
| 14 | `chat init --after RFC3339` | `chat.py:5323-5324` | none found | **Drop**, or pass a start time through the plugin's `backend_configuration` | S |
| 15 | Targeting an unregistered pinned pane (`target{}`) | `chat.py:687-700` | none found | **Drop**; adopt the pane first (docs only) | 0 |
| 16 | `event_command`, `transport_command`, `transport_socket` adapters | `chat.py:658-718,802`, `chat_socket.py` | none found | **Drop**; a compatibility plugin is optional | M if wanted |
| 17 | herdr-chat alias | `chat.py:5513-5515` | 0 | **Drop**; deprecation stub | S |
| 18 | Python chat process supervisors | `_command_supervisor.py`, `_event_command_supervisor.py`, `_command_anchor.py` | internal | **Archive** | 0 |
| 19 | Codex goal set/clear helper (`python -m agentctl.codex_goal`) | `codex_goal.py:249-330` | debugging only | **Drop** | 0 |
| 20 | Python `capabilities` JSON shape (`services` as strings, `headless` as an object) | `cli.py:267-271` vs `rs cli.rs:1412-1467` | n/a | **Document Rust's shape as the contract** | S |
| 21 | Exit 75 for refusals based on argument meaning (`send --model`, `read --output last`, `--startup-timeout 301`, `--mode headless --slot`) | `py/agentctl/errors.py:23-25` vs Rust usage errors (exit 2) | n/a | **Document Rust's exit 2 as the contract**; warn consumers, because a script that retries on 75 changes behaviour | S |

Rust-only features already in use, so nothing to port:
- agentcloud: 13 records on this host, which Python cannot even load;
- `inbox`: consumer-a's coordinator cron used `inbox watch --once` followed by `deliver --via print`, and the watcher is currently not re-armed;
- `--project-box`;
- the plugin chat host (gap retry, inspect, thread, publish, reply, short reply IDs) and the `provider-health.json` / `delivery-alarm.json` records, which consumer-a monitoring reads;
- `chat graceful-stop-main`;
- delivery confirmation that only counts evidence printed after the submit key.

### 3.2 Registry-format compatibility

| Record or artifact | Python | Rust |
|---|---|---|
| Flat schema-1 interactive record (herdr, herdr-pane, herdr-foreign, herdr-relay) | read and write | read and write. Handoff in both directions is cross-tested, and unknown `extension_metadata` survives archiving |
| Queue artifacts, `archive/<name>-<token>`, `.<name>.lock`, `.identity.lock`, target locks (`~/.local/state/agentctl/target-locks` plus the legacy `/tmp` root), `move.json` (agentctl-move/v1), `output.json`, `profiles.json` (agentctl-profiles/v1) | same format in both | same format in both |
| `agentctl-session/v2` and `v3` | read, and written back in the same nested form | **`list` fails for the whole registry** (exit 75) |
| turn-runner (headless) | full support | listed with capabilities `["status"]`; every operation refused, stop included |
| agentcloud | `list` exits 1 with a `record_error` row; status/send/stop exit 2; `herdr-agent status` exits 75 | full support |
| `<registry>/.inbox/<coordinator>` | ignored (dot prefix) | owner |
| One malformed row | other rows shown; exit 1 | nothing shown; exit 75 |

A registry can switch to Rust safely if it holds only flat interactive and agentcloud records. That is true of every registry on host-a. Required Rust work: rows 1, 3 (refusal message only) and 5 of the table in 3.1. Before archiving, run the same inventory on host-b:

```
grep -l -e '"turn-runner"' -e 'agentctl-session/v' <registry>/*/agent.json <registry>/archive/*/agent.json
```

### 3.3 Who runs what today

| Caller | Edition | Evidence |
|---|---|---|
| This checkout, `./bin/agentctl` | Python | `common/bin/engine-resolver:34` |
| Your PATH | Rust: `~/bin/agentctl -> ~/.local/bin/agentctl` (Sep 27, no `inbox`) and `~/.cargo/bin/agentctl` (Sep 24) | both print `agentctl 0.1.0` |
| consumer-a coordinator contact | Rust snapshot `agentctl-rust-9e09b6f70860` | consumer-a `coordinators.yaml:75` |
| consumer-a snapshot service | Rust snapshot `agentctl-rust-82f1ec42cc1e`, a different build | consumer-a `scripts/install-herdr-agent-snapshot.sh:32`, `scripts/systemd/consumer-a-herdr-agent-snapshot.service:19`. Neither snapshot is installed on host-a, so the infrastructure lives on host-b |
| consumer-a chat bridge (`herdr-chat-coordinator.service`, host-b) | Rust `chat run` with an external plugin and outbound helper. Python refuses `--herdr-bin` for chat (`py/agentctl/cli.py:252-255`), so this must be Rust | from configuration; the unit is not loaded on host-a |
| consumer-a monitoring | reads Rust-only `delivery-alarm.json` | consumer-a `ci-hub/health/chat_bridge_provider_down.py:10,19,98`; `chat_bridge_health.py:113-126` |
| consumer-a agents calling `consumer-a/agent-utils/bin/*` | Rust through cargo-runner (stale `bin -> rs/bin`, verified). That `rs/bin` has no wrkslots, wrkviz, parallel-experiment-runner, herdr-chat or herdr-subagents, which is why consumer-a hard-codes `py/bin/dagrun`, `py/bin/tick-hub` and `py/wrkslots/cli.py` | consumer-a `ci-hub/validate/start_unit.py:3016-3017`, `ci-hub/health/stale_tick_processes.py:166`, `ci-hub/validate/tests/test_published_tool_authority.py:501` |
| consumer-a tracked code | does not use py/agentctl, headless, herdr-subagents, reset/repair/migrate or MCP; turn-runner appears only in tests that refuse it | git grep |
| `tick-deliver` template | agentctl and tick-hub through one resolver | `skills/agent-utils-setup/templates/tick-hub/tick-deliver:13,20,22,35` |
| Setup skill | Python `chat launch`; says no build is needed | `skills/agent-utils-setup/SKILL.md:142-147,188-191,305-311,374` |
| Registries on host-a | 239 records: herdr 200, herdr-foreign 14, agentcloud 13, herdr-pane 10, herdr-relay 2. All interactive/herdr; none turn-runner, none v2/v3 | host survey |

### 3.4 Ordered work items

**Phase A: close the gaps that block a default switch**

| Step | Work | Effort | Depends on |
|---|---|---|---|
| A1 | Rust `list` per-row isolation (`subagents.rs:4111-4128`, `cli.rs:1160`), plus cross cases with a malformed row and an agentcloud row | S | none |
| A2 | Port Muse 1.4 composer and status recognition to `client.rs:719-759,1589` and `subagents.rs:1468-1472,1724-1728`, with the Python tests and a cross fixture | M | none |
| A3 | Guarded `forget` for non-Herdr records; clear refusal for v2/v3 rows that names the fix | S | none |
| A4 | Clean up the Rust surface (see list below) | S | the headless decision |
| A5 | Per-tool engine resolver (section 5.1) | S to M | none |
| A6 | Rust distribution: a provenance-stamped snapshot install, and `--version` that reports the edition and source fingerprint (section 5.2) | M | none |

A4 consists of:
- remove `--mode headless`, `--backend tmux`, `send --model`, `read --output since_turn` and `--since-turn`, or reword their refusals to "not supported" instead of pointing at "the worker extension" (`cli.rs:208-213,404,424-428,1072-1073,1163-1167,1198-1208`; `subagents.rs:1256`);
- refuse headless profiles at load time (`profiles.rs:470-497,523-537,571-572`);
- add the `start --harness` and `--mode` defaults and examples to help;
- add option descriptions to `herdr-agent --help` (`legacy_cli.rs:750`);
- add tests for `skill_install.rs`, which has none.

**Phase B: switch the default** (after A1, A2, A5, A6)
- B1 (S): make Rust the default for `agentctl` and `herdr-agent`. Keep Python reachable through the per-tool override for a sunset period, and keep the cross `agentctl` and `herdr-agent` nodes running.
- Announce the exit-code and `capabilities` contract (rows 20 and 21).
- Do this early. The Rust-only features are already in live use, and the Python default is actively wrong today (headline 3).

**Phase C: decouple** (can start now, in parallel)

| Step | Work | Effort |
|---|---|---|
| C1 | Move the docs out of `py/agentctl` (see list below) | S to M |
| C2 | Rewrite the guide as Rust-only (see list below) | S |
| C3 | Remove the conftest coupling: `py/tests/conftest.py:40,79-102`, and the agentctl imports in `py/tests/test_cross_env_is_hermetic.py:28-29,352-1717` | S |
| C4 | Validation: Rust-only components, an `archived/` rule, a mypy exclude (section 5.6) | S to M |
| C5 | Lint the Rust chat and inbox guides (`scripts/check_rust_packages.py:62-67`) and remove its Python exemption (`:312-314`) | S |

C1 consists of:
- move `README.md`, `USER_GUIDE.md` and `QUICKSTART.md` into `common/docs/agentctl/` or `rs/agentctl/`; `AGENT_USER_GUIDE.md` and `FOREIGN_USER_GUIDE.md` are links;
- repoint `rs/agentctl/README.md`, `src/embedded_userguide.md`, `src/embedded_agent_userguide.md` and `src/embedded_quickstart.md`;
- update `scripts/embed_userguides.py:188-197,310-319`, `py/tests/test_packaging_infrastructure.py:398` (51 links), `scripts/check_documented_defaults.py:177-200,400-417,448-460`, and the Cargo `readme`.

C2 consists of:
- remove `USER_GUIDE.md:21-34,803-838,860-911,915-948`;
- fix the stale chat claims at `:855-856` and `:880-889`;
- document `inbox`;
- fix `common/docs/agentctl/RELATED_WORK.md:41,153-157`.

**Phase D: your decisions and chat**
- D1: record the decisions in 3.1 as issue `agentctl-rust-only`.
- D2: Google Chat (open question 1).
  - Option (a), L: a new crate `rs/chat-subscription-google-chat`. It would hold a REST polling plugin built on `chat_subscription_plugin::serve_stdio` that emits `google.chat.message.v1`, and a native outbound helper ported from `chat.py:1470-1590`. These would be the first HTTP/TLS dependencies in `rs/Cargo.lock`. The helper must be native, because the helper is executed from a close-on-exec memfd, which rules out `#!` scripts. Reuse the conformance tests from `rs/chat-subscription-fake`.
  - Option (b), S: document that an external plugin is required.
- D3 (S, needed either way): document the outbound helper v1 contract (`send` and `ensure_reaction` requests, plus receipt and error fields). Today it exists only in `rs/agentctl/src/chat_runtime.rs:3100-3260`, although `embedded_chat_quickstart.md:106` claims the guide documents it.
- D4 (S, after D2): rewrite the setup skill's chat step to the Rust flow: register the coordinator, then `chat init` and `chat run --bridge-state`.
- D5: decide on MCP.

**Phase E: move the tests**
- E1 (M): port the scenarios from `cross/agentctl_differential.py` (1,957 lines) and `cross/herdr_agent_differential.py` (1,856 lines) into Rust integration tests that use the fake-Herdr fixture. Add the cases neither covers:
  - `start --brief` and `--file`, `read --output all|last`, `--max-attempts`;
  - `--slot` and `--project-box`;
  - the full `capabilities` document;
  - malformed, agentcloud and turn-runner rows;
  - Muse 1.4.

**Phase F: archive** (after B, C, D1, E, and the host-b survey). Use the checklist in 3.5. Close the issue and say what remains unverified: hosts other than host-a were not surveyed.

### 3.5 Archive-day checklist for `py/agentctl`

**Move.**
- `py/agentctl` (including `foreign/` and `foreign/tests`) goes to `archived/agentctl-python/`.
- The tests go with it: `py/tests/test_agentctl_*.py`, `test_herdr_agent.py`, `test_herdr_chat*.py`, `test_herdr_subagents.py`, `test_herdr_codex_goal.py`.
- Add a `DEPRECATED` README naming the Rust replacement, a deprecation line in `--help`, and the classifier `Development Status :: 7 - Inactive`.

**Resolver and dispatch.**
- Remove `py/bin/{agentctl,herdr-agent,herdr-chat,herdr-subagents}`.
- Replace `common/bin/herdr-chat` and `common/bin/herdr-subagents` with stubs that print the archived location and exit 2.
- Mark agentctl and herdr-agent as Rust-only in the per-tool table.
- Update `py/tests/test_repo_dispatch_wiring.py:15-35,73,88-89,92-113,123-126`, `py/tests/test_engine_resolver_source_pin.py`, and `setup:70-71,99-102`.

**Packaging.**
- `scripts/check_python_packages.py:157-169,187,203,1069-1086`
- `scripts/check_deps.py:30-33`
- `validation/packages.dag.yaml:8-12`
- `py/tests/test_cli_surface.py:20-25`

**Validation.**
- `validation/components.json:4-56`: drop the `py/` prefixes and the Python test files. This needs Rust-only component support.
- `validation/python-components.dag.yaml:13-32`.
- Add an `archived/` rule to `scripts/validate.py`.
- Exclude `archived/` from `Makefile:22-23` (strict mypy and `check_no_any` over `.`) and from `py/pyproject.toml`.

**Differential.** Once the Rust tests exist, delete:
- `validation/cross.dag.yaml:130-154`;
- in `cross/differential.py`: the import at `:124` and the entries at `:8390-8391`, `:12621-12622`, `:12711-12714`;
- the matching rows in `cross/README.md`.

**Docs.**
- `README.md:53,86,99,107-131,156-158`
- `rs/README.md:15`, `py/README.md:15,27`, `common/docs/README.md:33-37`
- `QUICKSTART.md:17-18,63`

**Skills.**
- `skills/agent-utils-setup/SKILL.md:87,92,142-147,188-191,305-311,370-375`
- `skills/herdr-agent/SKILL.md:14-19`
- `skills/agentctl` is already embedded by Rust (`embedded_agentctl_skill.md`).

**Compatibility names.**
- `herdr-agent` stays as the Rust `[[bin]]` in `rs/agentctl/Cargo.toml:33-35`.
- `herdr-subagents` and `herdr-chat` go, leaving stubs.
- Fix `README.md:127-130`, which says all three belong to the Python package, and the migration table at `USER_GUIDE.md:916-933`.

## 4. Small tools that could be made IDENTICAL

### 4.1 tick-hub: enforce identical editions

Already byte-identical:
- `list`, `json`, `state` and `tick` stdout;
- fired-state file bytes, including 12 phased-cadence tick points;
- config and state validation.

What differs today, and the fix for each:

| Difference | Evidence | Fix | Effort |
|---|---|---|---|
| `tick --dry-run` exists only in Python (ignores fired-state, reports pending items, refuses `--flush`). A consuming repository's wrapper passes it through (commit `c2891c31`) | `py/tick_hub/cli.py:335-342,420-432,461`; absent from `rs/tick-hub/src/cli.rs:274-304`; no Python test passes the flag | Port to Rust; add a Python test and a cross case | S |
| Gate process groups killed on SIGHUP/SIGINT/SIGTERM only in Python; under `timeout -s TERM`, Rust leaves an orphan | `py/tick_hub/probes.py:34-170,274` vs `rs/tick-hub/src/probes.rs:47-50` | Port; cross case requiring zero orphans. The shipped systemd unit (KillMode=control-group) already mitigates this | M |
| With fd 1 closed, Rust exits 0 and persists state; Python crashes with an AttributeError traceback (exit 1) | `rs cli.rs:469-513`; `py cli.py:453-455` | Rust fails without writing state; Python prints a clean error; add a closed-fd case beside the EPIPE case. Not reachable through `tick-deliver`, which redirects to a file | S |
| Read-only commands ignore write errors in Rust (EPIPE gives exit 0); Python prints a traceback (exit 1) | `rs cli.rs:519-597` (`let _ = writeln!`) | Make both exit nonzero cleanly | S |
| Rust help and quickstart lose all indentation, so the Rust quickstart's sample YAML does not load | `rs cli.rs:64-192` (backslash-newline continuations) | Write the exact argparse text out explicitly. Argparse output was byte-identical across Python 3.10, 3.12 and 3.14 | S |
| Tool-owned stderr differs: the no-flush trailer (Rust still says "dry-run"), usage-error format, numeric-error suffixes, I/O errors without the path | `py cli.py:477-482,248-271` vs `rs cli.rs:508-511,252-272,333-346,422-426,610-614` | Unify on Python's wording | M |
| `yaml` output bytes (PyYAML folded scalars vs serde_norway `\|-`) | `py/tick_hub/io.py:500-509` vs `rs/tick-hub/src/io.rs:830-834` | One canonical emitter, or declare and test equivalence only | M or S |
| `--userguide extra` exits 0 on Rust; an empty `NO_COLOR` disables colour only in Rust | `rs cli.rs:526-529,630`; `py cli.py:81-85` | Follow Python (no-color.org treats empty as unset) | S |
| Non-UTF-8 path arguments are mangled in Rust | `rs cli.rs:626-628` (`to_string_lossy`) | Use `OsString` | S |
| Python quickstart prints a literal `{{placeholders}}` | `py cli.py:166` | Fix | S |
| Both editions still report 0.2.0, unchanged since `5ef91c55` | `py/tick_hub/pyproject.toml:7`, `rs/tick-hub/Cargo.toml:3` | Bump both to 0.3.0 together | S |
| Docs: the guide calls the default run "a dry run" and omits `--dry-run` and `--report-pending`; no `RELATED_WORK.md`; README promises a `userguide` command that does not exist | `common/docs/tick-hub/USER_GUIDE.template.md:62-87`; `README.md:66` | Fix | S |

Enforcement to add in the tick-hub section of `cross/differential.py` (`:8996-9807`):
- byte-compare `--help`, every `<cmd> --help`, and `quickstart`;
- set `compare_stderr=True` throughout, with an explicit allow-list for messages that come from the parser libraries;
- add behaviour cases for `--dry-run`, SIGTERM orphans and closed fd 1;
- add tick-time due-ness for phased cadences (today only `list`/`json`);
- add the structured surface check and static version check from 4.5.

Alternative (S): retire Rust tick-hub. Nothing in-tree selects it, and the production template runs the resolver default. Choose this if Python will stay installed on every host anyway (open question 4).

### 4.2 cpuset-alloc

Not a candidate on its own: it ships inside the dagrun package and should follow dagrun to Rust. Until then:
- give Rust per-flag help with defaults (`rs/dagrun/src/cpuset_allocator.rs:28-38`);
- fix the `cpuset-alloc: : unknown command` message (`:629-631,678`);
- show the defaults in Python help (`py/dagrun/cpuset_allocator.py:431,456`);
- run `make cross` once with live scopes enabled.

### 4.3 herdr-agent

Follows agentctl, so Rust survives. It is not quite at parity today:
- Python `herdr-agent status` exits 75 on an agentcloud record, while Rust succeeds;
- neither edition's help describes every option, and Rust's describes none.

### 4.4 pr-landing-planner and herdr-run

Neither is a candidate:
- pr-landing-planner: retire Rust (section 2.4).
- herdr-run: the CLI surface is byte-identical, but the tool is not small and its risky part cannot be differentialed (section 2.5).

### 4.5 Enforcement for any tool that stays paired

This applies to tick-hub, and to dagrun and herdr-run during their transitions.
1. **Static version check:** a test that the `pyproject.toml` version equals the `Cargo.toml` version for each pair.
2. **Version bump rule:** bump the version in both editions, in the same commit, on every behaviour change. Put the rule in AGENTS.md.
3. **Build identity:** keep the first line of `--version` identical across editions so cross can compare it. Add the edition and source fingerprint behind `--version --verbose` or `capabilities`.
4. **Structured surface manifest:** each edition dumps its subcommands and long options as JSON, and cross compares them for equality against a per-tool allow-list of intended extensions. Replace the required-flag lists in `cross/differential.py:6884-7030` with this. Do not scrape `--help`: pr-landing-planner shows 18 spurious "Rust-only" flags caused purely by help layout.
5. **For identical tools only:** byte-compare help and quickstart, and compare tool-owned stderr.

## 5. Infrastructure changes

### 5.0 Do now, independent of the edition decisions

- Port the herdr-run worker cap to Rust and restart `herdr-run-server` (section 2.5).
- Make stale `bin -> rs/bin` links visible.
  - `setup` only rewires `bin` when it is re-run (`setup:120-133`), and the resolver never sees the stale case.
  - Add a warning in `rs/bin/cargo-runner` when it is reached through a top-level `bin -> rs/bin` link.
  - Tell consumer-a to repoint its link, or decide explicitly that Rust is its edition. Note that consumer-a also pins two different Rust agentctl snapshots.

### 5.1 `common/bin/engine-resolver`

- **One per-tool table** listing each tool's editions and its default. It drives the resolver, `setup:70-71,99-102`, the `rs/bin/cargo-runner:18-46` package map, and `test_repo_dispatch_wiring.py`.
- **A per-tool override,** for example `AGENT_UTILS_ENGINE_AGENTCTL=python`.
- **Deprecate the global `DAGRUN_ENGINE`.**
  - A tool that lacks the requested edition prints a note and runs its only edition, instead of exiting 127 (`:46-51`).
  - An explicit per-tool request for a missing edition still fails.
  - This also fixes Rust agentctl starting wrkslots with the variable inherited.
- **Allow Rust-only resolver tools.** Today `py/tests/test_repo_dispatch_wiring.py:73,88-89,92-113` require a Python console script for every resolver tool. The fix covers future agentctl, gh-paced, or wrkslotsd if kept.
- **Documentation:**
  - Document the switch in `README.md` and `skills/README.md`; today it appears only in `setup:13,130,135`.
  - Fix the header lists at `:4-6,17-19`.
  - Document or remove `CI_HUB_PYTHON` (`:44`). It is undocumented, untested, and named after a consumer component.

### 5.2 Rust distribution

- **Cost of cargo-runner today:**
  - about 0.2 s extra per call (agentctl: 0.13 s Python against 0.32 s Rust);
  - it needs cargo, flock, mktemp and sha256sum (`rs/bin/cargo-runner:49-54`);
  - it runs `cargo build --locked --release` (`:177`);
  - it fingerprints all of `rs/` (`common/bin/rs-source-fingerprint:9-13`), so a Rust edit that does not compile breaks agentctl for everyone in the checkout.
- **Add a snapshot install mode**, modelled on consumer-a's `agentctl-rust-<hash>` snapshots: build once into a hash-named directory with a provenance file. The resolver uses a verified snapshot that matches the current fingerprint and otherwise falls back to cargo-runner.
- **Replace `cargo install --path rs/agentctl`** at `README.md:111-113`; it is what leaves stale `0.1.0` binaries on PATH.
- **Update the "no build needed" claims:** `QUICKSTART.md:17-18,63` and `SKILL.md:87,92,188-191`.
- **Check publication first.** Before any deprecation release, find out whether crates.io or PyPI has these packages. That could not be verified from here.

### 5.3 Packaging checks

- Remove the agentctl, and later dagrun, Python projects from `scripts/check_python_packages.py`. Remove the pr-landing-planner (and possibly herdr-run) Rust crates from `scripts/check_rust_packages.py`.
- Correct the comment at `scripts/validate.py:180-183`. The index checks only reject foreign-language terms (`check_python_packages.py:1106-1112`, `check_rust_packages.py:765-770`); they do not check that the index lists every distribution shipped.

### 5.4 Docs renderer (`scripts/embed_userguides.py`)

- Change the agentctl package links and standalone docs (`:188-197,310-319`).
- Drop the Rust renders for pr-landing-planner and for whichever herdr-run edition loses (TOOLS at `:31-36`).
- Add gh-paced's `embedded_*.md` links.
- Update the link count in `py/tests/test_packaging_infrastructure.py:398` and the pins in `scripts/check_documented_defaults.py`.

### 5.5 `cross/`

- Replace the required-flag lists with the structured surface check from 4.5.
- Fix the docstrings: `differential.py:76-77` and `:6885-6890`.
- Update `cross/README.md`:
  - add agentctl to the examples and the "What is compared" table;
  - fix the "CLI surface" claim at `:39`;
  - correct "ignores `--herdr-bin`" at `:89-91`; Python refuses it.
- Add cases for the transition:
  - dagrun: `DAGRUN_COPY`, `--labels`, `-vv`, `quickstart` argument handling, normalized `run` stdout;
  - agentctl: malformed and agentcloud rows, Muse 1.4.
- Have something run the full `make cross` (#165) at least once before freezing dagrun and cpuset-alloc parity.
- Plan the `DIFFERENTIAL_TOOLS` removals (`:12615-12623`): pr-landing-planner, herdr-run (once one edition is chosen), agentctl, herdr-agent.

### 5.6 Validation groups

- **Allow Rust-only components.**
  - `scripts/run_component_tests.py:160,175-182,343-365` and `EXPECTED_COMPONENTS` (`:72-84`) require Python tests.
  - Rust tests are already tagged by DAG labels (for example `component-agentctl` in `validation/rust.dag.yaml`).
  - Then make gh-paced a component and delete the `validate.py:186-191` workaround.
- **Classify `archived/`.**
  - Add an `archived/` rule to `PREFIX_RULES` (`scripts/validate.py:146-242`): no checks, plus an explicit note.
  - Without it, any edit there selects every check (`:300-307`).
  - Exclude it from `Makefile:22-23`.
- **Soak dagrun on Rust.** Before the dagrun switch, run the validation graph (`scripts/validate.py:537-549`) on Rust dagrun.

### 5.7 README.md and AGENTS.md

- **`README.md`:**
  - `:65-67` promises `userguide` commands (only a `--userguide` flag exists for dagrun, tick-hub and pr-landing-planner) and a skill per tool (there is no gh-paced skill).
  - `:70-73` promises that edition extensions are listed, but the rows at `:79-83` list none for dagrun, cpuset-alloc or tick-hub.
  - `:105-106` calls gh-paced "the one Rust-only tool", ignoring wrkslotsd and the chat-subscription crates.
  - `:127-130` gets the ownership of the compatibility names wrong.
- **Add `RELATED_WORK.md`** for dagrun (with cpuset-alloc), tick-hub, pr-landing-planner and parallel-experiment-runner.
- **Add an "Editions" section to AGENTS.md** covering:
  - the rule: identical editions only for small, stable tools, enforced as in 4.5; otherwise one edition;
  - the per-tool table as the source of truth;
  - the version bump rule;
  - that `archived/` is not validated and must not be imported.

## 6. Open questions for you

1. **Google Chat on Rust.** Should Rust ship an in-repo Google Chat provider (L)? That reverses the shipped statement that Rust "does not ship a provider executable" (`py/agentctl/USER_GUIDE.md:25-27`) and adds the workspace's first HTTP/TLS dependencies. The alternative is to accept an external plugin, but then a new user has no in-repo chat path, which does not meet "fully self-contained".
2. **Dropping Python-only agentctl features.** Confirm dropping headless workers, reset/repair/migrate, agy, herdr-subagents (with its MCP server, WebSocket and policy hook), herdr-chat, and `agentctl mcp`. The "no users" evidence covers host-a only. Someone needs to survey host-b, where consumer-a's coordinator and bridge run.
3. **herdr-run's surviving edition.** Python is the documented default with more tests. Rust is what actually runs and is self-contained. Separately, do you want consumer-a's `bin -> rs/bin` link repointed now?
4. **tick-hub.** Is "a host needs no Python runtime" a goal? If yes, enforce identical editions now and move to Rust later. If no, retire Rust tick-hub (S).
5. **Rust distribution model.** Snapshot install, prebuilt release, or cargo-runner? Is cargo an acceptable hard dependency for agentctl from a checkout?
6. **parallel-experiment-runner and dagrun.** Which way do you want to re-home it: add `run --results-json` to Rust dagrun, port the runner, or vendor Python dagrun? This decides when dagrun's Python edition can be archived.
7. **External library users.** Does any submodule consumer use `dagrun.envcheck` or `DagConfig.known_failures` as a library API?
8. **Published packages.** Are any of these packages on crates.io or PyPI and in need of deprecation releases? Unverifiable from this host.
9. **rs/wrkslotsd.** Archive it now along with py/agentctl, or label it experimental?
10. **Sunset period.** How long should the Python escape hatch stay after agentctl switches to Rust, and should it point at `archived/` or at a pinned commit?
11. **Exit-code contract.** Do you accept Rust's exit 2 instead of Python's 75 for refusals based on argument meaning, with a note to consumers?