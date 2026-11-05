> Draft, 2026-10-11, by an agent ([opus 5.5]) for owner review. The owner's decision behind it:
> port 100% of the Python edition to Rust before archiving Python; nothing is dropped.
> CRITIQUE.md lists corrections that override this draft where they disagree.

# SPEC: agentctl, 100% Rust edition

- **Status:** draft for owner review, 2026-10-11. Baseline is HEAD `fbc8c497`. Repository files were not modified.
- **Slug:** `agentctl-rust-only`. The 2026-10-07 audit proposed it first (`ai_docs/transient/2026-10-07-edition-audit/REPORT.md` §3.4 D1). It has not been filed yet.
- **Inputs:** five area maps (core CLI, headless, MCP/foreign, chat, tests/distribution), all re-verified at HEAD. The audit's `REPORT.md` §3 and `CRITIQUE.md` were also used, and are superseded where noted below.
- **Redactions:** `consumer-a`, `host-a` and `host-b` stand in for the real names.
- **Effort scale:** S ≤ 1 agent-day, M 2–4, L 5–10, XL > 10.

**What changed since the audit (verified at HEAD):**
- **A1 (list row isolation):** done (`rs/agentctl/src/subagents.rs:7389-7420`, `cli.rs:1495-1500`).
- **A2 (Muse 1.4):** partly done. Footers, the versioned header and verified-process idle are in place (`client.rs:956-1046`). Still missing: paused and staged status, and Python's editor matching.
- **A3:** v2/v3 rows are now a clear `record_error` (`subagents.rs:4498-4505`). Turn-runner records still cannot be retired.
- **Audit "drop" rows 4, 6–9, 11–17 and 19:** superseded by the owner's port-everything decision.
- **Audit miss:** Python `anchor` rewrites a nested record as flat schema-1 and reports `migrated_from` (`py/agentctl/subagents.py:5313-5321,5366`).
- **Churn since 2026-10-04:**
  - `rs/agentctl/src/cli.rs`: 13 commits.
  - `subagents.rs`: 21.
  - `py/agentctl/USER_GUIDE.md`: 20.
  - `chat_runtime.rs` + `chat_service.rs`: 23.
  - Every core commit since 2026-10-08 changed both editions.
  - Python `foreign/` has had no commits since 2026-09-29.
- **Launcher cost (measured today):** Rust through cargo-runner takes 0.3–2.2 s per call, depending on load. Python takes 0.13 s. The snapshot binary run directly takes 0.002 s.

---

## 1. Goal and definition of done

**Goal.** One agentctl, written in Rust. It provides every capability of the Python edition (`py/agentctl`, console scripts `agentctl`, `herdr-agent`, `herdr-subagents`, `herdr-chat`) with equivalent behaviour. It starts no Python at runtime and includes a public Google Chat path. The Python edition is then archived and marked deprecated. Nothing is dropped.

### 1.1 Definition of done (all must hold)

1. **Every row of §2 is accounted for.** Each row is either `same` in Rust or covered by an approved difference in §1.2. The gate is `rs/agentctl/tests/fixtures/python-surface.json` (§4): every Python command, option, MCP tool and schema field maps to Rust or to a §1.2 entry.
2. **Rust is self-contained.**
   - The release binaries `agentctl`, `herdr-agent`, `herdr-subagents`, `herdr-chat` and `agentctl-google-chat` never execute Python.
   - Proof: the Rust contract suite and the provider crate's tests pass with `python3` and `python` on PATH replaced by guard stubs that fail the test if executed.
   - No Rust test writes a `#!/usr/bin/python3` fake. Today four do: `client.rs:3928,5084`; `client/tests/herdr_compatibility.rs:51`; `subagents/tests/dead_adoption.rs:416`.
3. **Google Chat works from this repository alone**, through an in-repo plugin and a native outbound helper. It does not depend on the Meta-internal Iris plugin.
4. **Python is archived.**
   - The code lives at `archived/agentctl-python/` with a DEPRECATED README.
   - Packaging carries `Development Status :: 7 - Inactive`, and the entry points print a one-line deprecation notice on stderr.
   - The final state is tagged `agentctl-python-final`.
   - `py/bin/{agentctl,herdr-agent,herdr-chat,herdr-subagents}` are removed.
   - `archived/` is excluded from validation, type checks and dispatch.
5. **Docs are Rust-owned.**
   - The quickstart and userguide live under `common/docs/agentctl/` and describe only shipped Rust behaviour.
   - Help describes every argument, with defaults, units and examples (AGENTS.md).
   - The guide has a "Differences from the Python edition" section listing §1.2.
   - `RELATED_WORK.md` compares MCP servers and Google Chat clients.
6. **Skills are updated:** `skills/agent-utils-setup` (chat step and build claims), `skills/agentctl-chat`, `skills/herdr-agent`.
7. **Tests are replaced.** Rust tests plus frozen goldens replace `cross/agentctl_differential.py`, `cross/herdr_agent_differential.py` and the 34 agentctl Python test files. `make validate-all` is green.
8. **Consumers are migrated.**
   - The engine table defaults the four names to Rust.
   - consumer-a is notified. It runs pinned Rust snapshots (consumer-a `coordinators.yaml:75`), so nothing it runs changes.
   - host-b is surveyed for turn-runner records, v2/v3 records, Python chat state and live `agent_runner.py` processes, and every hit is migrated.
   - The `tick-deliver` template keeps its meaning: exit 75 means queued (`skills/agent-utils-setup/templates/tick-hub/tick-deliver:37`).
9. **Issue `agentctl-rust-only` is closed**, with anything still unverified stated in the closing note.

### 1.2 Deliberate differences

Each is listed in the userguide. "Proposed" means the owner must approve it (§5). "Forced" means self-containment or the Rust architecture requires it. "Interim" means it closes when the chat owner lands a hook (M6g).

| id | Difference | Why | Kind |
|---|---|---|---|
| D1 | Refusals that can never succeed exit **2**, not 75. These are: `send --model` to an interactive agent; `read --output last\|since_turn` or `--since-turn` on an interactive agent; `start --startup-timeout` above 300; `--mode interactive --backend tmux`; `--mode headless --slot`. Headless runtime failures keep Python's 75, and stop with unavailable headless control keeps 69. | 75 means "safely queued, drain later" (`rs/agentctl/src/error.rs:3-4`; `skills/agent-utils-setup/templates/AGENTS.md:106`). `tick-deliver:37` exits 0 on 75, so with Python's code a prompt that can never be delivered is silently lost. The audit (row 21) recommended the same. | Proposed (Q1) |
| D2 | JSON is written as raw UTF-8, where Python escapes `\uXXXX`. The compact one-line pending object and sorted keys **do** match Python (M1). | Same meaning either way. The output writer is shared with chat-owned output. | Proposed (Q2) |
| D3 | `capabilities.services` is a list of objects `{name, origin, status}`, including `mcp` once ported. The `agentcloud`, `chat_subscriptions` and `build` keys are added. The `headless` object is byte-identical to Python's. | Rust's shape is already in use. Nothing found reads Python's string list. | Proposed (Q3) |
| D4 | `HERDR_SUBAGENTS_POLICY` names an executable that speaks `agentctl-subagents-policy/v1` JSON. A `.py` module path is refused with migration text. | Python imports a module in-process (`py/agentctl/foreign/policy.py:34-57`), which needs Python at runtime. | Forced |
| D5 | `python -m agentctl.codex_goal` becomes `agentctl codex-goal get\|set\|clear`, with the same flags, the same JSON and exit 1 with a `native goal:` prefix on error. | There is no Python module to run. | Forced |
| D6 | There is no importable Python API. The contract is the CLI's JSON plus the documented crate API. The archived package stays importable at the freeze tag. | Nothing in the tree imports it, apart from py tests and cross. | Forced |
| D7 | The Python chat supervisors (`_command_supervisor`, `_command_anchor`, `_event_command_supervisor`, `_launch_gate`) are not ported as helper processes. | The Rust host's clone3/pidfd supervision (`rs/chat-subscription-plugin/src/process.rs:996-1090`) fills that role. | Forced |
| D8 | Python bridge state is **migrated** (`agentctl chat migrate`), not shared. | Request keys differ: Python uses sha256(message id) (`chat.py:2331`); Rust uses sha256(JSON{channel_id,message_id}) (`chat_runtime.rs:11068-11074`). Rust also refuses changed content under an existing key (`:5132`). | Forced |
| D9 | The outbound helper and `transport_command` get 30 s in total. Python allowed 45 s for HTTP, 45 s for the token command and 60 s for the adapter. | Host cap (`chat_runtime.rs:390`). | Interim (M6g-6) |
| D10 | `chat run --observer-write-interval` is accepted and validated but has no effect, because Rust has no `output.json` observer. `chat tick` has no provider intake until M6g-3. | Architecture. | Interim |
| D11 | `chat launch` adopts and anchors the coordinator under `--name` (default `coordinator`), and reuses matching initialized state. Python leaves the coordinator unregistered (`chat.py:4630-4631`) and refuses to relaunch over existing state. | Fixes a Python bug. The setup skill assumes the coordinator is registered. | Proposed |
| D12 | `reply_mode: file` maps to `chat run --offer-reply-command`, but marker capture stays on. | No capture-off mode exists yet. | Interim (M6g-4) |
| D13 | Own-reply suppression uses a helper-assigned `messageId` prefix that the plugin filters, if the Google API allows it. The fallback is a configured prefix. Python matched its stored reply IDs. | The Rust host has no reply-ID feed to the plugin. | Proposed (Q11) |
| D14 | Python defects fixed (Q4): **a** `wait` treats `quota_exhausted`/`auth_error` as terminal (`sessions.py:346-349`). **b** `doctor` checks turn-runner rows through the runtime instead of reporting `pane-missing` for every tmux worker (`subagents.py:5653-5654`). **c** an empty `XDG_STATE_HOME` counts as unset. **d** the codex prompt is written without blocking, so no stdin deadlock. **e** the runner treats SIGHUP/SIGINT like SIGTERM, and the harness gets `PR_SET_PDEATHSIG`. **f** `agentctl mcp` answers an unknown method with -32601 and survives per-request errors (`mcp.py:140-145`). **g** herdr-subagents MCP returns operation errors as `isError` results (`adapter.py:97-111`; `lib.py:145-148`). **h** `herdr-subagents mcp --help` prints help (`foreign/cli.py:36-42`). **i** hints name real commands. **j** the policy's `backend()` answer is validated. **k** the WebSocket dedupe set is bounded, and slow clients are dropped. | Each is a latent failure. None is relied on. | Proposed |

### 1.3 Rust-only features kept (supersets, not differences)

agentcloud, `attach-self`, `--project-box`, `inbox`, the plugin chat host (gap retries, inspect, thread, sent, publish, short reply IDs, provider health and delivery alarms), `-V`, the `help` subcommand, and `chat --herdr-bin`. Proposed additions: `migrate --replace-runner`, `herdr-subagents repair`, and `quickstart`/`userguide` subcommands on herdr-subagents (Q7).

---

## 2. Capability matrix

- **Rust status:** `same` (identical); `differs` (present, different); `partial`; `absent`; `n/a`.
- **Milestone:** where the gap closes. `—` means nothing to port, apart from goldens in M8.
- IDs come from the area maps. The `RT-*` and `HL-LIFECYCLE`/`MCP`/`CHAT-SURFACE`/`HERDR-SUBAGENTS` umbrella rows are folded into the detailed rows.

### 2.1 Core CLI (`agentctl`, `herdr-agent`)

| id | Capability | Rust | Milestone |
|---|---|---|---|
| CLI-GLOBAL | `--registry`/`--state DIR`, `--herdr-bin`, before or after the subcommand; no subcommand prints help and exits 0 | same | — |
| CLI-VERSION | `agentctl 0.1.0` / `herdr-agent 0.1.0` | same; adds build provenance | M1 |
| CLI-DOCS | `quickstart`, `userguide`, `--userguide`, herdr-agent `userguide` | same (byte-identical, symlinked into `py/`, `lib.rs:24-29`) | M0 move, M7 rewrite |
| CLI-EXIT | Exit classes 0/1/2/69/75/76; semantic refusals exit 75 | differs (2) | M1 (D1) |
| CLI-JSONFMT | Compact one-line pending/possibly-submitted object; sorted keys; ASCII escaping | differs (`cli.rs:1001-1025,1067-1071`) | M1 (D2) |
| CLI-CAPS | `capabilities` document | differs (`headless:null`, `cli.rs:1675-1730`) | M3d, M4 (D3) |
| CMD-START-INTERACTIVE | Interactive `start` and all its options, profiles, slots, timeouts | same; help omits defaults (`cli.rs:296-304`) | M0 help |
| CMD-ADOPT-MOVE-RENAME-ANCHOR-DOCTOR | `adopt`, `move`, `rename`, `anchor [--replace]`, `doctor [--repair-labels]` | same, except the nested-to-flat migration in `anchor` | M1 |
| CMD-LIST | Per-row isolation and `record_error`; per-row capabilities | same for interactive rows; turn-runner rows status-only | M3d |
| CMD-STATUS | Record plus queue, goal and live probe | same for interactive; turn-runner branch absent | M3d |
| CMD-SEND-DRAIN | `send`/`drain`, message IDs, quarantine, misroute read-back | same; `send --model` refused | M3d |
| CMD-READ | `--lines`, `--output tail\|all\|last\|since_turn`, `--since-turn` | partial | M3d |
| CMD-WAIT | `wait --timeout`; headless polling | partial | M3d |
| CMD-REVIVE | `revive [NAME] --all --dry-run …` | same; help prints the default twice (`cli.rs:194-196`) | M0 help |
| CMD-STOP | `stop` with recovery flags and the `Recovery command:` advice | partial (turn-runner refused, `subagents.rs:1955-1966`) | M1 dead worker, M3d full |
| CMD-ATTACH-PAUSE-RESUME | Herdr focus; tmux select/attach; worker pause marker | partial | M3d |
| CMD-GOAL-BIND | `goal`, `bind-session` | same | — |
| CMD-PROFILES | `profiles` (agentctl-profiles/v1, including headless, agy and agentcloud) | same (headless profiles parse but cannot launch) | M3d |
| CMD-SKILL | `skill install --harness … --force` | same; `skill_install.rs` has 0 tests | M1 tests |
| MUSE-STATUS | Muse 1.4 `paused` and `staged` statuses, in Python's order | partial (`subagents.rs:3422-3428,3614-3620`) | M1 |
| MUSE-PROOFS | Editor segment matching; transcript turn count; verified-process prompt checks | differs (`client.rs:972-1094`) | M1 |
| NESTED-RECORDS | Read and write back agentctl-session/v2 and v3; `anchor` migrates them | absent (refused, `subagents.rs:4498-4505`) | M1 |
| REG-FLAT | Flat schema-1 record; unknown keys preserved | same | — (M8 goldens) |
| STATE-FORMATS | Queue, locks, rename/move journals, misroute and read-back logs, archive | same | — (M8 goldens) |
| HERDR-AGENT | `herdr-agent` direct and managed modes; all commands and defaults | same; help lacks option descriptions (`legacy_cli.rs:855-866`) | M0 help |
| CODEX-GOAL | Native goal get, set and clear | partial (get only, `codex_goal.rs:252`) | M1 (D5) |
| ATTACH-SELF-STUB | Python's refusal stub | n/a (Rust has the real command) | — |
| PY-LIBRARY | Importable Python package API | differs | D6 |

### 2.2 Headless workers (turn-runner adapter)

| id | Capability | Rust | Milestone |
|---|---|---|---|
| HL-01 | `start --mode headless` (with profiles): turn-runner record, runtime home, identity check | partial (flags parsed, then refused at `cli.rs:1269-1271`) | M3d |
| HL-02 | tmux presentation (session `subagents`, exact targets, pane break, tmux absent) | absent | M3c |
| HL-03 | Herdr presentation for runners (shared workspace, fresh tab, `backend.json`) | absent (the Herdr client verbs exist) | M3c |
| HL-04 | Runtime home and on-disk formats: `registry.json`, sidecars, locks, `_archive`, `WORKSPACE_LOST.json` | absent | M3a |
| HL-05 | Durable turn queue: sequence numbers, claim, quarantine, inflight recovery, pause gating | absent | M3a |
| HL-06 | Transcript sentinels, answer files, `events.jsonl` | absent | M3a |
| HL-07 | Long-lived runner, staged activation, wrapper that keeps the pane open | absent | M3a |
| HL-08 | Harness process group, watchdog, identity-checked signalling (#30 pidfd-process-ownership) | absent | M3a |
| HL-09 | Codex `exec`/`exec resume` adapter | absent | M3a |
| HL-10 | Muse `exec` adapter and strict JSONL validator | absent (only argv checks, `profiles.rs:416`) | M3b |
| HL-11 | agy adapter, quota and auth classification, preflight | absent | M3b |
| HL-12 | Headless `send`, `send --model`, `SUBAGENT_EFFORT` | differs (refused, `cli.rs:1399-1405`) | M3d |
| HL-13 | `read` last, since_turn, tail and all on transcripts | differs (refused, `cli.rs:1443-1446`) | M3d |
| HL-14 | Turn-runner `status`/`list`; `gc` | differs (`probe_error` names Python) | M3c/M3d |
| HL-15 | Headless `wait` | absent | M3d |
| HL-16 | Headless `attach` (Herdr tab or exact tmux window) | absent | M3d |
| HL-17 | `pause`/`resume` plus the runtime `automation-paused` marker | differs (record flag only) | M3d |
| HL-18 | Headless `stop` (STOP marker, grace, terminate, double archive, receipt relocation) | differs (refused) | M1 dead worker, M3d |
| HL-19 | `reset` | absent | M3e |
| HL-20 | `repair` (recreate presentation) | absent | M3e |
| HL-21 | `migrate --backend herdr\|tmux` (pause, ack, stage, activate; rollback) | absent | M3e |
| HL-22 | Headless to Codex TUI conversion (`--mode interactive`) | absent | M3e |
| HL-23 | TUI mode inside the runtime (shared delivery state machine) | absent (`agent.rs` is reusable) | M3e |
| HL-24 | Backend and mode selection; the `SUBAGENTS_*`/`HERDR_*` environment catalogue | absent | M3c |
| HL-25 / POL-1 | `HERDR_SUBAGENTS_POLICY` hook | absent | M3c (D4) |
| HL-26 | `worker_rpc` boundary | absent | M3f |
| HL-31 | `capabilities.headless`; headless exit mapping (75, 69) | differs | M3d |
| HL-32 | Chat-side process supervisors | differs | D7 |
| HL-33 | procstat (`/proc/PID/stat` including session; ENOENT versus unreadable) | differs (private parser in `client.rs:290-338`) | M3a |

### 2.3 `agentctl mcp`

| id | Capability | Rust | Milestone |
|---|---|---|---|
| MCP-0 | Line-delimited JSON-RPC over stdio, protocolVersion 2024-11-05, 1 MiB per request, requests without an id ignored | absent | M4 |
| MCP-call | `tools/call` validation order and messages; argv translation; text = stdout+stderr; `isError` | absent | M4 |
| MCP-T01…T18 | `agent_{start,adopt,send,list,status,read,wait,stop,move,pause,resume,goal,bind_session,drain,migrate,reset,repair,attach}` with `additionalProperties:false` | absent. T03 `model` and T06 `last`/`since_turn` need M3d; T15–T17 need M3e | M4 |

### 2.4 `herdr-subagents`

| id | Capability | Rust | Milestone |
|---|---|---|---|
| SUB-0 | Executable, dispatcher, exact `--version`/`--userguide` | absent (no `rs/bin/herdr-subagents`) | M5 |
| SUB-1…SUB-9 | `up`, `send`, `read`, `status` (fixed-width table), `down [--grace --all-dead --force]`, `inbox [--inspect\|--drain] [--clear-composer]`, `reset`, `migrate [--to --mode]`, `backend [--set]` | absent | M5 |
| SUB-10 / HL-28 | `turn` (blocking; exits 0, 2, 3, 4) | absent | M5 |
| SUB-11 / HL-29 | `keeper ensure\|retire\|status` | absent | M5 |
| SUB-12 / HL-30 | `mcp` (stdio server plus WebSocket) | absent | M5 |
| SUB-RC | `recreate_window` (repair) | absent | M3e (runtime), M5 (CLI) |
| FMCP-0 | MCP dialect: protocol 2025-06-18, sorted keys, error codes | absent | M5 |
| FMCP-T1…T9 | `subagent_{up,send,read,status,down,reset,recreate_window,migrate,list}` | absent | M5 |
| WS-1 | WebSocket events on 127.0.0.1:`${SUBAGENTS_MCP_WS_PORT:-18765}` | absent | M5 |
| DOC-1 | herdr-subagents userguide and docs | differs (Rust prints text promising features it lacks) | M0/M7 |

### 2.5 Chat

| id | Capability | Rust | Milestone |
|---|---|---|---|
| CHAT-01 | Google Chat REST polling intake (filter, pagination, 60 s overlap, allowlist, skip own replies and empty text) | absent (no provider; `plugins.discovered=[]`) | M6a |
| CHAT-02 | Threaded reply send (`requestId`, `REPLY_MESSAGE_OR_FAIL`) | partial (host only) | M6a |
| CHAT-03 | ACK reaction with optional `reaction_user` reconciliation | partial (host only) | M6a |
| CHAT-04 | Token from env or `token_command` (per call, bounded, never echoed) | partial | M6a |
| CHAT-05 | `reply_mode` tagged or file | differs | M6b (+M6g-4, D12) |
| CHAT-06 | `outbound_mode` enabled or disabled | differs (`outbound_enabled`) | M6f translator |
| CHAT-07 | `chat launch` | absent | M6c (D11) |
| CHAT-08 | `chat init --after` | differs (option rejected) | M6b |
| CHAT-09 | `chat run --interval/--observer-write-interval` | differs | M6b (+M6g-2, D10) |
| CHAT-10 | `chat tick` polls the provider | differs | M6g-3 |
| CHAT-11 | `chat status` (offline) | differs (fails on Python state) | M6b |
| CHAT-12 | `chat context --request --limit --cursor` | absent | M6b (+M6g-1) |
| CHAT-13 | `chat history` | partial (`chat sent` caps at 100) | M6b |
| CHAT-14 | `reply` without a reply ID; 12-hex prefixes | differs | M6b (+M6g-5) |
| CHAT-15 | Chat `quickstart`/`userguide`/`--version`/`--userguide` with Google Chat content | partial | M6a/M6b |
| CHAT-16 | Unregistered `target{}`; `agent_name` as a live Herdr agent name | absent | M6f |
| CHAT-17 | `event_command` streaming adapter with REST reconcile | absent | M6d |
| CHAT-18 | `transport_command` | absent | M6d (D9) |
| CHAT-19 | `transport_socket` | absent | M6d |
| CHAT-20 | `herdr-chat` command (default state `.herdr-chat`) | absent | M6e |
| CHAT-21 | `--state` after `chat` means the bridge state; default `<registry>/.chat` | differs | M6b |
| CHAT-22 | Python bridge state and config migration | absent | M6f (D8) |
| CHAT-23 | Python chat supervisors | differs | D7 |
| CHAT-24 | Zero-install built-in transport | absent | M6a (`install`) |

### 2.6 Distribution

| id | Capability | Rust | Milestone |
|---|---|---|---|
| DIST-1 | Resolver default `python` (`common/bin/engine-resolver:34`); `py/bin` launchers | n/a | M7 |
| DIST-2 | Wheel with 4 console scripts and the guides as package data | differs: cargo-runner snapshots; install text is wrong (`README.md:116`, `rs/README.md:20-24`) | M7 |
| DIST-3 | Runs read-only inside a wrkslots box | absent: cargo-runner writes locks (`rs/bin/cargo-runner:106-110`; `py/wrkslots/sandbox.py:253-264`) | M7 |
| DIST-4 | `herdr-chat` and `herdr-subagents` command names | absent (`rs/bin/cargo-runner:35-37` handles only agentctl and herdr-agent) | M5, M6e |

---

## 3. Milestones

### 3.0 Order, parallelism and collision rules

```
M0 foundations ──┬── M1 core gaps ───────────────────────────────┐
                 ├── M2 contract harness ── M3a ─┬─ M3b           │
                 │                               └─ M3c ─ M3d ─ M3e ─ M3f
                 │              M2 ── M4 (accepted after M3e)     │
                 │              M3c+M3e+M4 ── M5                  │
                 └── M6a ── M6b ── {M6c, M6d, M6e, M6f}   M6g (chat owner, parallel)
M1 + M3 + M4 + M5 + M6 ── M7 switch ── M8 test migration + freeze ── M9 archive
```

There are three parallel tracks after M0:
- **Core and headless:** M1, M3.
- **Surfaces:** M4, M5.
- **Chat:** M6. M6a is a new crate and can start at once.

Rough total: 100–130 agent-days.

**Collision rules (apply to every milestone):**
1. **Logic goes in new files.** Hot files (`cli.rs`, `subagents.rs`, `client.rs`, `USER_GUIDE.md`, `cross/*_differential.py`) get only *hook commits*.
   - About 60 lines or fewer, and one per milestone or sub-milestone.
   - Announced to both core teams first.
   - Rebased immediately before pushing (`git pull --rebase origin main`).
   - Never bundled with logic.
2. **Registry integration uses child modules of `subagents.rs`**, following `subagents/cloud.rs` and its `is_cloud()` hooks. Private helpers (`lock`, `load`, `save`, identity transaction, archive) are reused without widening their visibility. New `HerdrClient` verbs go in a `client/` child module, which costs one `mod` line in `client.rs`.
3. **Chat-owned files are never edited:** `rs/agentctl/src/chat_*.rs`, `plugins.rs`, `credentials.rs`, `rs/chat-subscription*/`, `embedded_chat_*.md`. Hooks are requested (M6g), and each interim difference is documented.
4. **Dependency additions** to `rs/*/Cargo.toml` and `rs/Cargo.lock` get their own commit, announced to the chat agent.
5. **Doc edits** to `USER_GUIDE.md` go in small separate commits after the feature has landed.
6. **Output ordering:** never rely on `serde_json::Map` order. tick-hub enables `preserve_order` (`rs/tick-hub/Cargo.toml:18`), so a workspace build changes the order. Sort explicitly.

---

### M0 Foundations (M; start now)

- **Scope:** CLI-DOCS (move), DOC-1 (part), help gaps (CMD-START-INTERACTIVE, CMD-REVIVE, HERDR-AGENT), the decision issue, the dual-landing policy, Rust-only validation components, the `archived/` rule, Python-free Rust test fakes, and the python-surface gate (§4).
- **Design:**
  - File `agentctl-rust-only` in `.minibeads/issues/`, where #30 pidfd-process-ownership lives (`.minibeads/issues/agent-utils-30.md`), or on GitHub (Q24). Re-scope agent-utils-30 to M3a.
  - Add an "agentctl editions" section to AGENTS.md carrying the §4 policy.
  - **Docs move, as one announced pure-rename commit:**
    - `git mv py/agentctl/{USER_GUIDE,QUICKSTART,README}.md common/docs/agentctl/` and leave symlinks at the old paths. setuptools dereferences them, and `importlib.resources` keeps working (`py/agentctl/cli.py:30-33`).
    - Repoint `rs/agentctl/README.md` and `src/embedded_{userguide,quickstart,agent_userguide}.md`. They stay symlinks under `rs/`, so `rs-source-fingerprint` still hashes them.
    - `CHAT_USER_GUIDE.md` stays where it is until M7.
    - Update `scripts/embed_userguides.py:188-197,310-319`, the 51-link pin at `py/tests/test_packaging_infrastructure.py:399`, `scripts/check_documented_defaults.py:177-200,400-417,448-460`, `common/docs/README.md:33-35`, `README.md:160-161` and `rs/agentctl/Cargo.toml:4`.
  - **Validation:**
    - Add `("archived/", frozenset())` to `scripts/validate.py` PREFIX_RULES, with a self-test.
    - Add `mypy --exclude '^archived/'` (`Makefile:21-23`) and add `archived` to `scripts/check_no_any.py` `_IGNORED_DIRS`.
    - Let `scripts/run_component_tests.py` accept an empty `test_files` when a DAG step carries `component-<name>` (`:160,172-183`).
  - **Help text (one hook commit to `cli.rs` and `legacy_cli.rs`):**
    - Add the defaults for `start --harness` (codex) and `--mode` (interactive).
    - Fix the duplicated default in `revive`.
    - Add descriptions and defaults to every herdr-agent option.
    - Leave the "worker extension" strings until M3d.
  - Replace the four `#!/usr/bin/python3` fakes in Rust tests with `/bin/sh` scripts.
  - Add the python-surface generator and its gate test (§4).
- **Files:** `common/docs/agentctl/*`; `py/agentctl/*.md` (now symlinks); `rs/agentctl/src/embedded_*.md`; `scripts/{validate,run_component_tests,check_no_any,embed_userguides,check_documented_defaults}.py`; new `scripts/dump_python_agentctl_surface.py`; `Makefile`; `AGENTS.md`; `cli.rs` and `legacy_cli.rs` (help only); four Rust test files.
- **Collision:** `USER_GUIDE.md` (20 commits in 7 days). Land the rename when neither team has an unpushed guide edit, after announcing a time. A later edit made through the symlink changes the target file, so only branches already in flight conflict.
- **Tests:**
  - `validate.py --self-test` covers `archived/`, and `run_component_tests --self-test` covers a Rust-only component.
  - The link pins and documented-defaults checks pass.
  - quickstart and userguide output is md5-identical across editions.
  - In a scratch worktree, `git mv py/agentctl …`, then `cargo build -p agentctl`.
- **Acceptance:** `make validate-all` is green, run bare with no pipe. The scratch-worktree build succeeds. Rust tests pass on a PATH without `python3`.

### M1 Core gaps (L; parallel with M0; 8 independent commits, all S except nested records)

- **Scope:** MUSE-STATUS, MUSE-PROOFS, HL-18 (dead-worker part), NESTED-RECORDS, CLI-EXIT, CLI-JSONFMT, CLI-VERSION, CODEX-GOAL, CMD-SKILL tests. For CMD-LIST, only verify that it is done.
- **Design:**
  - **Muse:**
    - New `rs/agentctl/src/client/muse_editor.rs`. Port these line for line from `py/agentctl/client.py`:
      - `muse_verified_process_composer` (166-170)
      - `muse_verified_process_goal_paused` (172-181)
      - `_muse_editor_segments`, `_muse_editor_matches` and `_muse_prompt_transcript_count` (227-337)
      - the verified-process prompt checks (288-306)
    - Change `muse_prompt_in_composer`, `muse_prompt_is_exact_composer` and `muse_prompt_transcript_count` (`client.rs:1053-1094`) to use them. Text echoed in an assistant answer can then no longer satisfy the post-Enter proof.
    - Status order becomes paused, idle, staged, working at `subagents.rs:3422-3428` and `3614-3620`, as in `py/agentctl/subagents.py:1594-1626`. Paused and staged keep the input queued (75).
  - **Dead-worker retirement (interim for HL-18):**
    - `stop NAME --expected-token T` on `adapter: turn-runner` reads the runtime `registry.json` row under `LOCK_SH`. It checks `runner.pid` against `runner_started_at` and `/proc/PID/stat`.
    - If the runner is alive or the check is inconclusive, refuse with exit 69 and Recovery advice (`subagents/stop_advice.rs`).
    - If the runner is provably dead, rename `<registry>/<name>` to `archive/<name>-<token>` (renameat2, no replace) and relocate the receipt paths (`sessions.py:406-425`). Report the presentation target that was left in place.
    - Replace the message at `subagents.rs:1963`.
    - M3d replaces this with the full stop. It then stays as the path for an unreadable runtime home.
  - **Nested records:**
    - New `subagents/nested.rs` decodes with Python's exact key-set and contract checks (`subagents.py:430-535,701-912`) into `AgentRecord`, plus `#[serde(skip)] nested_storage` and `session_source`.
    - `save()` re-emits the nested form and refuses a changed launch projection.
    - `anchor` migrates to flat schema-1 (asserted sessions become `goal_session_id`) and adds `migrated_from` to its result.
    - One hook replaces the refusal at `subagents.rs:4498-4505`. Stop and adoption-recovery raw-byte hashes are unchanged.
  - **Exit codes:**
    - Default per Q1: keep 2 (D1), plus a table-driven test.
    - If the owner picks 75 instead: return `Failure::Agent(Delivery)` at `cli.rs:1269-1271,1400-1405,1435-1446` and move the >300 startup check out of clap.
  - **JSON:**
    - Write the pending/possibly-submitted object on one compact line (`cli.rs:1001-1025,1067-1071`).
    - Add `write_sorted_json`, which sorts keys explicitly and recursively, and switch the core and herdr-agent call sites to it.
    - `legacy_cli.rs:536-544` relies on `Map` order today, which flips under a workspace build.
    - Chat-owned output paths are not touched.
  - **Provenance:** add `capabilities.build = {edition:"rust", source_fp}`, using `option_env!` and a new `build.rs` with `rerun-if-env-changed`. The commit is left out of the binary so unrelated commits do not force a rebuild. The first line of `--version` is unchanged.
  - **Codex goal:** port `set_goal`/`clear_goal` (`codex_goal.py:238-290`) into `codex_goal.rs`. Add a documented `agentctl codex-goal [--stdio|--command-json JSON] [--timeout S] get|set|clear SESSION_ID` (Q9).
  - **skill_install:** add unit tests for bounds, symlink refusal and atomic replace.
- **Files:**
  - New: `client/muse_editor.rs`, `subagents/nested.rs`, `subagents/turn_runner.rs` (it starts with retirement and grows in M3d), `build.rs`.
  - Hooks: `client.rs` (`mod` line and 3 call sites), `subagents.rs` (2 status maps, `read_record`/`save`, stop dispatch, message), `cli.rs` (write paths, capabilities `build`, `Commands::CodexGoal`), `legacy_cli.rs`, `codex_goal.rs`, `skill_install.rs`.
- **Collision:** high. `client.rs` and `subagents.rs` carry the Herdr 0.9.3 work (f281f5cd) and the identity/misroute work. Land each item as a new-file commit followed by a separate hook commit, after telling the Muse and adoption owners.
- **Tests:**
  - **Ported:**
    - `py/tests/test_agentctl_client_launch.py:238-330`
    - `py/tests/test_herdr_subagents.py:335-1040` (nested) and `:1514-1555`
    - `py/tests/test_agentctl_adopt.py:1665-1673`
    - `py/tests/test_agentctl_identity.py:470-490`
    - `py/tests/test_herdr_codex_goal.py` (13)
    - `py/tests/test_agentctl_skill_install.py` (3)
    - `py/tests/test_agentctl_sessions.py:1139`, adapted to retirement
  - **New Rust:**
    - A paused-goal Auto-review frame holds input.
    - An echoed prompt does not satisfy the transcript proof.
    - An exit-code table covering every refusal class.
    - Byte goldens for the pending line and drain key order, built both `-p agentctl` and `--workspace`.
    - Retirement refuses a live runner and a recycled PID (faked `/proc` starttime).
  - **Cross:** add groups `muse14_status`, `nested_records` and `exit_codes` to `cross/agentctl_differential.py`. Both editions must agree, apart from D1.
- **Acceptance:**
  - Rust reads, writes back and anchors v2/v3 records exactly as Python does (cross check).
  - No Rust message refers users to the Python edition for v2/v3.
  - `capabilities.build` is present.
  - All of the tests above are green.

### M2 Rust contract harness (L; after M0)

- **Scope:** X02 (fixtures), the X01 harness with record mode and a first batch of groups, the X03 guard properties, and fakes for later milestones.
- **Design:**
  - **Test target:** `[[test]] name="contract" path="tests/contract/main.rs" harness=false` in `rs/agentctl/Cargo.toml`. `main()` dispatches on `argv[0]`:
    - `fake-herdr`: port of `_FAKE_HERDR`, `cross/herdr_agent_differential.py:89-702`, keeping its `state.json` schema.
    - `fake-muse-skills` (`:704-732`) and `host-cli-guard` (`:760-767`).
    - New: `fake-tmux`, `fake-codex`, `fake-muse`, `fake-agy`.
  - **Fixtures and products:**
    - Each case directory symlinks those names to `current_exe()`.
    - The fake Muse runtime is a copy of `current_exe`, giving it a distinct inode.
    - Products under test come from `CARGO_BIN_EXE_agentctl`/`herdr-agent`; `herdr-subagents`/`herdr-chat` are added later.
    - Because nothing extra is published, `NON_PUBLISHED_RUST_BINARIES` is unchanged.
  - **Guard rules:** copy them from `cross/herdr_agent_differential.py:1243-1290`, and add `python3` and `python` to the guarded commands.
  - **Record mode:** `Harness.invoke` and `Report` write normalized outcomes to `rs/agentctl/tests/golden/{agentctl,herdr-agent}-contract.jsonl`.
  - **Global-option guard test:** a clap test that enumerates the global options of `Cli::command()` replaces the source scraping at `py/tests/test_cross_env_is_hermetic.py:1663-1700`.
  - **Validation:** add an `agentctl.contract` step to `validation/rust.dag.yaml` with label `component-agentctl`.
  - **First groups ported:** orientation, invalid_cli, lifecycle, handoff_and_pending, registry_and_interop.
- **Files:** new `rs/agentctl/tests/contract/*`; `rs/agentctl/Cargo.toml` (`[[test]]`); `validation/rust.dag.yaml`; `cross/herdr_agent_differential.py` (an additive record mode).
- **Collision:** `cross/*` gets 12 to 20 commits a week. The record mode is additive inside `Harness`; announce it.
- **Tests:** harness self-tests check that the guard catches `python3`, user git config and ambient `*_BIN` variables.
- **Acceptance:** `cargo test -p agentctl --test contract` passes with no Python on PATH. The first five groups match the recorded Python goldens. `make validate` runs the target.

### M3 Headless runtime and the turn-runner adapter (XL)

- **Scope:** HL-01 to HL-26, HL-31, HL-33; the runtime half of SUB-RC.
- **Layout.** This settles a naming conflict between the area maps.
  - `rs/agentctl/src/workers/` holds the runtime and knows nothing about the agentctl registry:
    - `home.rs`: `RuntimeHome` and `WorkerConfig::from_env`, with no process globals.
    - `registry.rs`, `inbox.rs`, `transcript.rs`, `events.rs`, `procid.rs`, `supervise.rs`, `runner.rs`.
    - `harness/{mod,codex,muse,agy}.rs` and `presentation/{mod,tmux,herdr}.rs`.
    - `lifecycle.rs`, `migrate.rs`, `tui.rs`, `policy.rs`, `rpc.rs`.
    - It is not named `foreign/`, because Rust already uses `herdr-foreign` for adopted agents (`subagents.rs:624`).
  - `rs/agentctl/src/subagents/turn_runner.rs` integrates with the registry.
  - Hidden subcommands: `agentctl __turn-runner --home DIR NAME` and `agentctl __worker-rpc`.
- **M3a runtime core (L):** HL-04 to HL-09, HL-33.
  - **Byte-compatible files:**
    - `registry.json` rows have 17 keys in Python's dataclass order (`foreign/lib.py:407-490`), indent 2, ASCII escaping, and an explicit key order.
    - Never add keys to registry rows, because Python writers drop unknown keys.
    - Rust-only data goes in sidecar `state/<name>/runner.json`, schema `agentctl-headless-runner/v1`: {edition, pid, boot_id, starttime_ticks, executable device/inode}, the shape of `client.rs:51-64`.
  - **Locking:** flock interoperates with Python's `.registry.lock` (EX for read-modify-write, SH for reads), `.headless-turn.lock`, `.harness-<name>.lock` and `state/.events.lock`.
  - **Queue:** sequence numbers are allocated inside the registry lock. A claim is renameat plus a directory fsync. The quarantine sidecar is written before the rename. The `possibly_submitted` vocabulary is kept.
  - **Transcript markers** (`agent_runner.py:436-899`) are constants and a public contract: coordinators grep for `TURN-DONE`.
  - **Runner wrapper:** `env … <current_exe> __turn-runner … 2>>runner.stderr.log; …; exec sleep infinity`, the same shape as `lib.py:2231-2257`. Refuse an executable path ending in ` (deleted)`.
  - **Runner loop:** `poll()` on a signal self-pipe, with 50 ms timers for markers and 500 ms for the inbox. SIGTERM, SIGHUP and SIGINT all kill the harness group and write `RUNNER DOWN` (D14e).
  - **Supervision** (closes #30 pidfd-process-ownership for headless):
    - Spawn and reap:
      - The runner sets `PR_SET_CHILD_SUBREAPER`.
      - The harness starts with `process_group(0)`, and `pre_exec` sets `PR_SET_PDEATHSIG(SIGKILL)` and checks `getppid`.
      - `pidfd_open` runs right after spawn.
      - Exit is observed with `waitid(P_PIDFD, WNOWAIT)`, then `killpg` runs while the zombie leader still pins the group, then the leader is reaped.
    - The CLI never calls `killpg` on a group it does not parent. Its stop sequence:
      1. Open a pidfd for the runner, then re-verify boot ID and starttime.
      2. Write STOP and wait 10 s.
      3. `pidfd_send_signal` SIGTERM, wait 2 s, then SIGKILL.
    - Opt-in cgroup leaf (`AGENTCTL_HEADLESS_CGROUP=auto|off`). `status` reports the containment in use.
    - Copy the pidfd helpers (`client.rs:342-412`, `rs/chat-subscription-plugin/src/process.rs:996-1090`); never edit those files.
  - **Codex adapter:** argv exactly as `agent_runner.py:220-241`. The prompt is written without blocking from inside the poll loop (D14d).
- **M3b adapters (M):** HL-10, HL-11.
  - **Muse:** limits of 100k events, 1 MiB per event, 64 MiB transcript and 4 MiB answer. The schema-v1 state machine and its error strings are copied verbatim (`agent_runner.py:59-69,268-333,586-763`).
  - **agy:** promote `regex` to a normal dependency, so `(?i)` and `\b` match Python's semantics. Add the `~/.gemini/antigravity-cli/settings.json` preflight (`lib.py:359-404`).
- **M3c presentation, lifecycle and config (L):** HL-02, HL-03, HL-14 (gc), HL-24, HL-25.
  - **tmux driver:** run tmux through the bounded control-command pattern. ENOENT becomes a synthetic 127 result with a one-time warning. Targets are exact (`=S:=W`).
  - **Herdr presentation:** use `HerdrClient` (`client.rs:1495-2990`), cache the workspace in `backend.json`, open a fresh tab per worker, and never close the shared workspace.
  - **gc:** 20 s startup grace, a snapshot on workspace loss, and a live TUI pane is never reaped.
  - **Config:** `WorkerConfig::from_env` with the same parse failures and the same list of variables passed to children.
  - **Policy:** the policy hook becomes an executable (D4).
  - **Missing Herdr calls** (workspace get, pane process-info) go in a new `client/workers_ext.rs`.
- **M3d agentctl integration (M):** HL-01, HL-12 to HL-18, HL-31.
  - **One hook commit to `subagents.rs`:** about 10 one-line `is_turn_runner()` calls next to the `is_cloud()` sites (status_record ~7326, attach ~7548, wait ~7725, stop ~9270/9299, pause/resume, `supported()` 1949-1967, list capabilities).
  - **One hook commit to `cli.rs`:**
    - `Start` (`1269-1271`) calls `turn_runner::start`.
    - `Send` (`1399-1405`) and `Read` (`1433-1446`) get turn-runner branches.
    - The `capabilities.headless` object becomes byte-identical to Python's.
    - The "worker extension" strings at `cli.rs:296,497,517-521` are reworded.
  - **Exit mapping:** runtime failures exit 75; unavailable control at stop exits 69 with doctor advice (`py/agentctl/cli.py:500-509`).
  - **Visibility:** `profiles::validate_muse_headless_arguments` becomes `pub(crate)` (`profiles.rs:416`).
- **M3e reset, repair, migrate and TUI (L):** HL-19 to HL-23, SUB-RC.
  - New commands `Commands::Reset`, `Repair` and `Migrate{--backend herdr|tmux, --mode interactive|headless}`. Each runs under the per-name lifecycle lock and the require-automation check (`sessions.py:313-322`).
  - `migrate.rs` ports the marker protocol and every error code verbatim (`lib.py:988-1069,2975-3640`).
  - `tui.rs` reuses `agent.rs` drain, send, status and read, with the runtime state directory as the queue root. `agent.rs:4386-4400` already accepts the foreign message shape.
  - Proposed superset `migrate --replace-runner`: a same-backend staged handoff that moves a live Python runner onto a Rust runner without losing the conversation (Q7).
- **M3f worker-rpc (S):** HL-26.
  - `rpc.rs` Request and Response types match `worker_rpc.py:43-97` exactly.
  - The in-process path sits behind a `WorkerRuntime` trait, which is also the test seam that replaces Python's `_worker` monkeypatch.
  - The hidden `__worker-rpc` subcommand exists for the cross differential.
  - Optional soak shim in `py/agentctl/sessions.py:39` (Q18).
- **Files:**
  - New: `workers/*`, `subagents/turn_runner.rs`, `client/workers_ext.rs`.
  - Hooks: `subagents.rs`, `cli.rs`, `lib.rs` (`pub mod workers;`), `client.rs` (`mod`), `profiles.rs`, `prompt_time.rs` (`pub(crate)` on `:127-150,205`), `Cargo.toml` (`regex`).
- **Collision:** the new files carry no risk. `subagents.rs` and `cli.rs` are high-risk; limit them to the M3d hook commits, landed with the core teams' agreement. The Python runtime has had no commits since 2026-09-29, so the target is stable.
- **Tests:**
  - **Ported:**
    - `py/agentctl/foreign/tests/test_agent_runner.py` (27) and `test_backends.py` (60)
    - `py/tests/test_agentctl_sessions.py` headless cases: 209, 299–520, 683–720, 731, 751–803, 1139, 1355
    - `py/tests/test_agentctl_adopt.py:1260,1324,1969-1997`
    - the turn-runner cases in `test_agentctl_lifecycle.py`
  - **New Rust:**
    - A setsid escapee is killed in cgroup mode.
    - A double-fork orphan is reaped by the subreaper.
    - A recycled PID is never signalled.
    - An 8 MiB prompt causes no deadlock.
    - Muse limits are enforced.
    - tmux is absent.
    - A Python-written `registry.json` is rewritten byte-identically, using a fixture.
  - **Cross:**
    - the worker-rpc request-corpus differential
    - groups `headless_lifecycle` and `headless_migrate`, using fake codex, muse and tmux
    - mixed editions: a Rust CLI drives a Python runner, and a Python CLI drives a Rust runner
- **Acceptance:**
  - `capabilities.headless` equals `{"backends":["herdr","tmux"],"harnesses":["codex","agy","muse"]}`.
  - All ported tests pass without Python on PATH.
  - In the cross harness, a Python-started worker is sent to, read with `--output last`, migrated and stopped by Rust.
  - #30's acceptance items are met for headless, or re-filed precisely.

### M4 `agentctl mcp` (M; after M2; accepted after M3e)

- **Scope:** MCP-0, MCP-call, MCP-T01 to T18, the `mcp` entry in CLI-CAPS.
- **Design:**
  - **Shared framing,** `rs/agentctl/src/mcp_stdio.rs`, used by M5 as well:
    - Reads with `read_until` under a 1,048,577-byte cap. An oversized line is discarded up to the next newline and answered with an error carrying id null.
    - One flushed line per reply. stdout carries protocol traffic only.
  - **agentctl dialect,** `mcp.rs`:
    - `protocolVersion` 2024-11-05; `serverInfo` {agentctl, `CARGO_PKG_VERSION`}; no reply to a request without an id.
    - The 18-tool table and schemas are copied from `mcp.py:22-61`, and the validation order and messages from `mcp.py:66-101`.
    - argv is built in schema order: the name, or the session for `bind_session` only, as a positional; then `--key=value`; then `-- TEXT`.
  - **Execution:** each call runs `/proc/self/exe` (falling back to `current_exe`) with stdin null and stdout+stderr captured. `isError` means a non-success exit. The Rust-only global options are forwarded. Calls are handled one at a time, as in Python.
  - No rmcp: it needs tokio, and the workspace has no async runtime.
  - Wiring: one `Commands::Mcp` variant, plus `{name:"mcp", origin:"built_in", status}` in capabilities.
- **Files:** new `mcp_stdio.rs` and `mcp.rs`; `cli.rs` (1 variant, 1 dispatch arm, 1 capabilities line); `lib.rs`.
- **Collision:** low. `mcp.py` has been unchanged since 2026-09-26.
- **Tests:**
  - **Ported:** `py/tests/test_agentctl_mcp.py` (4: `:13-31`, `:34-38`, `:41-46`, `:49-68`); `test_agentctl_sessions.py:668-680`; `test_cross_env_is_hermetic.py:1810-1845` (stdin is not inherited).
  - **New:** oversized line; non-object request; missing `jsonrpc`; unknown method returns -32601 (D14f); the server survives a crashing child.
  - **Cross:** an `mcp` group comparing parsed replies.
- **Acceptance:** `tools/list` matches Python's frozen schema after key sorting. All 18 tools work end to end against fake Herdr, including migrate, reset and repair once M3e is in. capabilities lists `mcp`.

### M5 `herdr-subagents` (L; after M3c, M3e and M4's framing)

- **Scope:** SUB-0 to SUB-12, the SUB-RC CLI, FMCP-0, FMCP-T1 to T9, WS-1, DIST-4 (part).
- **Design:**
  - **Binary:** a third `[[bin]] herdr-subagents` at `src/workers_main.rs` (Q16). A separate crate would need a dependency cycle with `agent.rs` and `client.rs`.
  - **CLI:**
    - A clap front end over `workers::lifecycle` with the 12 subcommands.
    - Exact `--version` (`herdr-subagents 0.1.0`) and `--userguide`. Errors print `error: MSG` and exit 1.
    - Human output is exact; status table widths follow `agent_status.py:28-73`.
    - Default home: `$HERDR_SUBAGENTS_HOME`, else `$XDG_STATE_HOME/herdr-agent/foreign`.
    - Adds `quickstart`, `userguide`, help for every argument, and the proposed `repair NAME`.
  - **turn:** tails from the size taken before dispatch, uses the regex at `agent_turn.py:44`, and exits 0, 2, 3 or 4.
  - **keeper:** its loop runs `env HERDR_SUBAGENTS_HOME=… <self> status`, with no Python child.
  - **`workers/mcp.rs`:** protocol 2025-06-18; `serverInfo` herdr-foreign-subagents 0.1.0; keys sorted explicitly; error codes as in `server.py:380-454` except D14g.
  - **`workers/ws.rs`:**
    - std `TcpListener` on 127.0.0.1. A port ≤ 0 disables it. A bind failure is logged and MCP keeps serving.
    - Handshake by hand: 8 KiB header limit, 10 s timeout. Frames use 7-, 16- and 64-bit lengths.
    - Sources: `events.jsonl` tailed every 0.5 s and transcript `TURN-DONE` lines every 1 s. Dedupe uses an LRU of 65,536 keys. Each client has a write timeout.
    - SHA-1 from `sha1 = "0.10"` (one new lock package) plus the already-locked `base64 0.22`, or hand-rolled (Q25).
  - **Wiring:**
    - an `rs/bin/herdr-subagents` link
    - the cargo-runner case arm (`rs/bin/cargo-runner:35-37`) and `setup:71,99-102`
    - `RUST_TOOLS` in `py/tests/test_repo_dispatch_wiring.py:15-37`
    - `scripts/check_rust_packages.py:64-68` (bins and command userguide)
    - `validation/components.json` prefixes and the engine-resolver header
- **Files:** new `src/workers_main.rs` and `workers/{compat_cli,turn,keeper,mcp,ws}.rs`; `Cargo.toml`; `rs/bin`; `cargo-runner`; `setup`; `scripts`; dispatch tests.
- **Collision:** low. Edits to cargo-runner select the full validation contract (`scripts/validate.py:167,230`), so run `make validate-all`.
- **Tests:**
  - **Ported:** `py/agentctl/foreign/tests/test_mcp_server.py` (12); `test_backends.py:276-307` (the keeper's environment).
  - **New:**
    - a CLI golden per subcommand
    - the RFC 6455 vector `dGhlIHNhbXBsZSBub25jZQ==` → `s3pPLMBiTxaQ9kYGzzhZRbK+xOo=`
    - frame lengths 125, 126 and 65536
    - dedupe across both sources; a bind failure keeps MCP up
    - stdio errors -32600, -32601 and -32700
    - a stale `TURN-DONE` from before a reset is ignored
  - **Cross:** a herdr-subagents text-output group.
- **Acceptance:** `DAGRUN_ENGINE=rust bin/herdr-subagents --help` exits 0. Every subcommand matches Python on the cross corpus. Both MCP tool lists equal Python's, apart from approved schema fixes.

### M6 Chat (XL; M6a can start at M0)

**Boundary.**
- The chat agent owns the host. Plugin protocol v1 and helper wire v1 are not changed, because the Meta-internal integration depends on them.
- Inside Meta, a Rust Iris subscription plugin and outbound helper (fbsource `fbcode/scripts/newton/agentctl-iris-plugin`, unlanded) serve Google Chat for Rust agentctl. They cannot be published.
- The public provider uses the already-reserved capability `chat-subscription.google-chat.polling` (`plugins.rs:1470`; `scripts/check_rust_packages.py:328`). Discovery refuses duplicate capabilities, so check that the Iris manifest uses a different ID.

- **M6a provider crate (L):** CHAT-01 to CHAT-04, CHAT-15 (part), CHAT-24.
  - **Crate:** `rs/chat-subscription-google-chat`, package `agentctl-google-chat`.
    - lib `chat_google` with a `config` feature that pulls in no HTTP dependencies; agentctl links only that feature.
    - One native binary, `agentctl-google-chat`:
      - With no arguments it is the plugin, run through `chat_subscription_plugin::serve_stdio`. On a TTY it prints help instead.
      - `outbound` is the one-shot helper. It must be native ELF, because the host runs it from a sealed memfd (`chat_runtime.rs:3467-3530`).
      - `install [--home]`, `quickstart` and `userguide`.
  - **Plugin configuration:** schema `google.chat.polling.v1`.
    - `poll_interval_seconds`: 0.1–86400, default 3600.
    - `start_after`, `exclude_message_ids`, `token_env` or `token_command`, `transport` (`rest|command|socket`), `intake` (`poll|event_command`), `skip_empty_text`.
  - **Polling:**
    - The first batch is `[checkpoint]`.
    - Each page is `GET v1/{space}/messages?pageSize=100&orderBy=createTime asc&filter=createTime > "after"[&pageToken]`. Batches are at most 256 events and 512 KiB.
    - Senders outside the allowlist and empty text are filtered inside the plugin, because the core poisons the generation otherwise (`rs/chat-subscription/src/lib.rs:1310-1334`).
  - **Cursor and overlap:**
    - The cursor is base64url JSON {v, after, page_token, high_water, seen, seq}, at most 8192 bytes and unique per batch. After the last page, `after = high_water − 60 s`.
    - The `seen` set stops a message whose resource changed from being emitted again. The host refuses changed content under an existing key (`chat_runtime.rs:5132`) and a reused cursor (`:5100`). A naive port of Python's overlap therefore breaks on the first edit, or on the bridge's own 🤖/✅ reaction.
    - Ordinary overlap never produces a protocol Gap.
    - `provider_payload` is the immutable subset of `google.chat.message.v1`, at most 256 KiB.
  - **Errors:** 401/403/429/5xx and network failures are retryable Error frames. The host reconnects in 1–60 s (`chat_service.rs:63-64`). Plugin stderr is discarded (`process.rs:1196`), so diagnostics travel in frames. Cancellation wakes any sleep.
  - **Helper:**
    - A send with a thread posts `…?requestId=…&messageReplyOption=REPLY_MESSAGE_OR_FAIL` and checks the returned name prefix.
    - A send with thread null posts a root message (for `chat publish`).
    - `ensure_reaction` does optional `--reaction-user` reconciliation, treating pagination as an error, and accepts ✅.
    - Outcomes: 400/403/404 → `not_applied`; 401/429 → retryable; timeout or 5xx after the body was written → `unknown`, retryable.
    - The whole operation must finish within 30 s.
  - **Token:** from env, or from `token_command`: literal argv, 16 KiB cap, ASCII 0x21–0x7e, CRLF trimmed, never logged, fetched per request, never falling back to stale env.
    - The helper's environment is cleared (`chat_runtime.rs:2946-2948`), so the translator lists PATH, HOME and the token variable.
    - The plugin's baseline environment has no proxy variables (`plugins.rs:67-76`), so the translator also lists `HTTPS_PROXY` and `NO_PROXY`.
  - **HTTP stack:** blocking `ureq` with `rustls` (ring) and certificate roots (Q10). These are the first HTTP/TLS crates in `rs/Cargo.lock`, which has 89 packages today; expect 25 to 35 more. The offline package smoke must prefetch them (`scripts/check_rust_packages.py:565-582`).
  - **`install`:** writes the manifest (`agentctl-plugin-manifest/v1alpha1`), stages a single-link copy, renames it into place atomically, and prints a ready `chat.json`.
  - The helper's v1 field schema goes in the crate README, which closes audit D3.
- **M6b chat CLI parity (M):** CHAT-05, 08, 09, 11 to 15, 21, in a new `rs/agentctl/src/legacy_chat.rs`.
  - **`--state` rewrite:** in `main.rs`, after the `chat` token, `--state` becomes `--bridge-state`. `bridge_state` becomes optional with default `<registry>/.chat` (Q13). Passing both is refused.
  - **`init --after`:** written into a private 0600 temp config copy, which is then passed to the unchanged `chat_service::initialize`.
  - **`run --interval`:** a provider override (M6g-2). Interim: the `AGENTCTL_GOOGLE_CHAT_POLL_INTERVAL` variable, which the translator always lists.
  - **`--observer-write-interval`:** validated at 60–3600 s, no effect (D10).
  - **`chat --version`/`--userguide`.**
  - **Prefixes and history:** 12-hex prefixes for close, history and context. `history` uses a request-scoped, uncapped `sent_replies` and a read-only reader for Python state.
  - **`context`:**
    - Resolve the key, take the source from `inspect_request`, and pin the helper through `CommandOutboundTransport::new`.
    - Call an additive helper action, `thread_history {channel_id, thread_id, before, limit, cursor}`, and validate the reply with a port of `chat_context.py:59-151`. This needs M6g-1.
  - **`status`:** detects Python state (`bridge.json` with after/high_water/started_at) and names `chat migrate`.
  - **reply_mode:** mapped per D12.
- **M6c `chat launch` (M):** CHAT-07.
  1. Check the environment and that the pane is idle.
  2. Build argv with `subagents::harness_arguments` (`subagents.rs:741`) and run the harness in the pane's foreground group.
  3. Wait for Herdr to report the agent, then `adopt` (`:6154`) and `anchor` (`:5133`) it under `--name` (D11).
  4. Translate the config, then initialize the state, or reuse it if it matches.
  5. Run `current_exe … chat run` in its own process group, logging to `bridge.log` (2 × 1 MiB).
  6. Wait with pidfd and signalfd.
     - If the coordinator exits: SIGTERM the bridge, join it, and retire the dead adopted record.
     - If the bridge exits: stop the coordinator and print the log tail.

  Only public APIs are used.
- **M6d adapters (M):** CHAT-17 to CHAT-19, all inside the provider.
  - **`event_command`:** the adapter runs as the plugin's child inside the host-supervised group. The cursor is a composite. An adapter gap triggers an internal REST recovery scan, not a protocol Gap. REST reconciles also run periodically.
  - **`transport_command`:** literal argv, 60 s inside the plugin, 8 MiB stdout and 64 KiB stderr, and the process group is killed on overrun.
  - **`transport_socket`:** `UnixStream` with `SO_PEERCRED`, a 0700 parent directory, 1 MiB requests, 8 MiB responses, 60 s.
- **M6e `herdr-chat` (S):** CHAT-20. `[[bin]] herdr-chat` at `src/legacy_chat_main.rs` calls `cli::main` with the chat prefix, default state `.herdr-chat` and prog name `herdr-chat`. Add the cargo-runner arm, the `rs/bin` link and the dispatch test.
- **M6f migration (M):** CHAT-06, 16, 22. Command: `agentctl chat migrate [--from DIR] [--bridge-state NEW] [--name] [--readmit-open|--abandon-open]`.
  1. Refuse while Python holds `.run.lock` or `.bridge.lock` (`flock LOCK_NB`).
  2. Parse the Python config strictly.
  3. Translate it with the `chat_google::config` translator (shared with launch and init):
     - `space` → `channel_ids`
     - token, transport and event options → `backend_configuration` plus helper arguments and environment
     - `outbound_mode` → `outbound_enabled`, with `ack_reaction` null when disabled
     - `target`/`agent_name` → a registry name, adopting and anchoring if needed
     - `start_after` = Python's `after`
     - `exclude_message_ids` = handled IDs at or after `start_after`, within 64 KiB
  4. Open requests are re-admitted by default. Unsent replies are refused unless `--abandon-open` is given (Q12).
  5. Rename the old directory to `OLD.python-<UTC>`.
  6. Initialize the new state and print a JSON summary.
- **M6g chat-owner hooks (requested; on the chat owner's schedule):**
  1. `CommandOutboundTransport::exchange` → `pub(crate)` (`chat_runtime.rs:2920`).
  2. `RunSettings` provider overrides.
  3. A bounded provider pass in `tick`; the plugin emits a heartbeat after each drained poll.
  4. `reply_capture: markers|command_only`.
  5. A reply-alias read accessor.
  6. Optional: exact own-reply filtering, a context hint in thread prompts, and a helper deadline above 30 s.

  Each closes D9, D10, D12 or CHAT-14.
- **Files:**
  - The new crate; `rs/Cargo.toml` members; `rs/Cargo.lock`.
  - `legacy_chat.rs`, `legacy_chat_main.rs`, `main.rs`.
  - `cli.rs`: `Chat` (version and userguide), `ChatState` (optional bridge state), `ChatInit` (`--after`), `ChatRun` (two options), and new `Launch`/`Context`/`History`/`Migrate`.
  - `rs/agentctl/Cargo.toml` (`[[bin]]`); `validation/{components.json,rust.dag.yaml,rust-packages.dag.yaml}`; `scripts/{check_rust_packages,embed_userguides}.py`; `rs/bin`; cargo-runner.
- **Collision:** high for the `cli.rs` chat structs: agree that hook commit with the chat agent before landing it. The crate itself carries no risk. Announce the `Cargo.lock` commit.
- **Tests:**
  - **Ported:**
    - `test_herdr_chat_transport.py`: `:60`, `91`, `115-157`, `183-232`, `960`, `1079`, `1377-1525`, `1583-1723`
    - `test_agentctl_chat_ack.py` (26), `_context.py` (13), `_context_cli.py` (10), `_launch.py` (24), `_socket.py` (18)
    - `_input.py` (31: framing and event schema)
    - `test_herdr_chat.py` (10)
    - the `--after` cases in `_lifecycle.py` and `_deferred.py`
    - the behavioural subset of the 354 chat-core tests that Rust lacks (durable-echo cursor advance, bounded storage, multiple replies, operation limits), coordinated with the chat agent
    - The supervisor tests stay in the archive (D7).
  - **New:**
    - the `rs/chat-subscription-fake` conformance suite run against the provider
    - a fake HTTP server bound to 127.0.0.1:
      - an edit or reaction in the overlap is not re-emitted
      - a page over 512 KiB is split
      - a sender outside the allowlist is filtered, not poisoned
      - the first batch is a checkpoint; each cursor is unique
      - cancellation works during sleep
    - a helper round-trip through `chat_runtime`'s decoders
    - `install` output accepted by `discover()`
    - migration from Python fixtures: lock refusal, no duplicates, re-admission, rename
    - unit tests for the argv rewrite
  - **Cross:** a translator group (Python config → Rust config) and `chat status` on migrated state.
- **Acceptance:**
  - `agentctl-google-chat install`, then `chat init`, `chat run` and `chat launch`, delivers and replies end to end against the fake server.
  - `plugins.discovered` lists `google-chat`.
  - The python-surface gate shows no unmapped chat entry.
  - A Python bridge directory migrates without duplicates or loss.
  - One live Google Chat run on host-b is recorded as unverified until done (Q26).

### M7 Resolver and distribution switch (L; after M1, M3, M4, M5, M6)

- **Scope:** DIST-1 to DIST-3, DOC-1 and CLI-DOCS (rewrite), the skills, consumers.
- **Design:**
  - **Engine table:** `common/bin/engines.tsv` (tool, editions, default), read by engine-resolver, setup, cargo-runner and `test_repo_dispatch_wiring.py`. Resolution order:
    1. `AGENT_UTILS_ENGINE_<TOOL>` is a demand. If that edition is missing, exit 127 and name the archive.
    2. `DAGRUN_ENGINE` is a preference that applies only to tools that have the edition; for a tool with one edition, print one note.
    3. Otherwise use the table default.

    This settles CRITIQUE item 18. It also decouples validation's dagrun (`scripts/validate.py:540,607`) from agentctl's engine.
  - **Switch:** agentctl, herdr-agent, herdr-chat and herdr-subagents default to `rust`. `python` stays listed for the override until M9.
  - **cargo-runner fast path:**
    - Read `<snapshots>/<tool>/current` (binary sha256, `source_fp`, `rustc -vV`), which `./setup rs` writes atomically. Compute the fingerprint read-only (about 43 ms). Verify the sha256, then exec. No lock, no writes, no cargo.
    - If the snapshot is stale and the checkout is read-only or cargo is missing, exit 3 with the exact rebuild command. Never run stale bytes and never fall back to Python (Q21).
    - Snapshot GC never removes a binary that a live process references: headless runners, and the chat service, which prints `current_exe` into prompts (`chat_runtime.rs:6548,6606`).
    - Fix the target path at `py/wrkslots/tests/accept_coordinator_box.sh:130-137`.
  - **Install:** `./setup rs --install DIR` creates `DIR/agentctl-rust-<binsha12>/bin/{agentctl,herdr-agent,herdr-subagents,herdr-chat,agentctl-google-chat}` plus a `PROVENANCE` file (commit, `source_fp`, sha256, `rustc -vV`). This matches the pinned-snapshot convention consumer-a already uses. Rewrite `README.md:116` and `rs/README.md:20-24`; `cargo install agentctl` installs an unrelated crates.io crate.
  - **Docs:**
    - Rewrite the Python-only parts of `USER_GUIDE.md` (`:22-42,1051-1090,1108-1130,1160-1185`) and merge `CHAT_USER_GUIDE.md` into the Rust chat guide (chat owner).
    - Add the "Differences from the Python edition" section.
    - Update `README.md:53,55,87,100,113-134` and `rs/README.md:15`.
    - Add the MCP and Google Chat comparisons to `common/docs/agentctl/RELATED_WORK.md:41,153-157`.
  - **Skills:** `skills/agent-utils-setup/SKILL.md:87,92,142-147,188-197,305-311,374` (Rust default, provider install, build requirement); `skills/agentctl-chat/SKILL.md:12-20`.
  - **Consumers:**
    - Announce the exit-code and capabilities contract, and notify consumer-a.
    - Survey host-b: `grep -l -e '"turn-runner"' -e 'agentctl-session/v' <registry>/*/agent.json <registry>/archive/*/agent.json`, plus the Python chat state directories and `pgrep -f agent_runner.py`. Migrate each hit with `migrate --replace-runner` or `chat migrate`.
    - Soak for at least 7 days with the Python override available.
- **Files:** `common/bin/{engine-resolver,engines.tsv}`, `setup`, `rs/bin/cargo-runner`, `py/tests/test_engine_resolver_source_pin.py`, `test_repo_dispatch_wiring.py`, docs, skills.
- **Collision:** the infrastructure files are quiet but select every validation group, so run `validate-all`. The guide rewrite is high-risk: do it after the core teams are working Rust-first.
- **Tests:**
  - **Resolver:** a Rust-only tool ignores `DAGRUN_ENGINE=python` with a note; a Python demand for a Rust-only tool exits 127 without running `py/bin`; the table drives dispatch.
  - **Fast path:** a verified snapshot runs with cargo absent and the checkout under `chmod -R a-w`; a stale snapshot in a read-only checkout is refused.
  - **Install:** a provenance round-trip.
  - **Box:** a real coordinator-box run (manual).
- **Acceptance:**
  - `bin/agentctl` with no environment runs Rust.
  - It works read-only inside a default wrkslots box without cargo.
  - Overhead with a verified snapshot is at most 50 ms.
  - The host-b survey is clean or migrated.

### M8 Test migration and freeze (XL; after M7)

- **Scope:** the rest of T01–T07 and T09, all 21 X01 groups plus no-host-cli, the 9 X02 groups, X03, X04, and the final python-surface.
- **Design:**
  - Port the remaining Python tests into sibling modules: `src/agent/tests/queue_contract.rs`, `src/subagents/tests/{lifecycle_contract,adoption_contract}.rs`, `identity.rs` additions and `profiles.rs` tests. One Rust fake client replaces `FakeManagedClient`.
  - Track every Python test function in `rs/agentctl/tests/python-test-map.tsv`, marked `ported:<rust test>`, `covered-by:<rust test>` or `retired:<reason>`. A freeze-time script checks that every collected Python test ID appears in it.
  - Announce the Python freeze date. From then on, new scenarios go only into the Rust contract suite.
  - Run record mode for the final goldens.
  - **X04 fixtures:** a generator in `archived/agentctl-python/tools/` writes deterministic Python state to `rs/agentctl/tests/fixtures/python-state/<scenario>/` with `PROVENANCE`. The state covers flat records, queues, archives, v2/v3 records, turn-runner homes and chat bridges. Rust tests run list, status, send, drain, stop, revive, headless status and chat status on copies of it.
  - Freeze `python-surface.json`.
  - Tag `agentctl-python-final` after `make validate-all` passes with both cross agentctl nodes green.
- **Test volume:** T01 75, T02 49, T03 (a subset of 42), T04 58, T05 232, T06 about 45, T07 28, X03 34, plus 30 differential groups.
- **Collision:** high on `cross/*` and `py/tests/test_cross_env_is_hermetic.py` until the freeze. Announce the cutover date at M7.
- **Acceptance:** the test map is complete; goldens and fixtures are committed; the tag exists; the contract suite is green with no Python on PATH.

### M9 Archive (M; after M8)

- **Scope:** A01, A02, X05, P01, P02, V03.
- **Steps:**
  1. `git pull --rebase`. Confirm that nothing touching `py/agentctl` or `cross/*agentctl*` is in flight.
  2. **Move the code.**
     - `git mv` the package (with `foreign/`), the 31 `py/tests` files, the conftest target-lock fixture, both cross harnesses and the agentctl half of the hermetic test into `archived/agentctl-python/`.
     - Replace the doc symlinks with frozen regular files and fix the LICENSE link depth.
     - Add the DEPRECATED README: a Rust replacement for each command, the tag and SHA, and how to run the frozen edition through `git worktree add`.
     - Add the classifier, the `DEPRECATED:` description prefix and the stderr notice.
  3. Remove `py/bin/{agentctl,herdr-agent,herdr-chat,herdr-subagents}` and make those engine rows Rust-only.
  4. **Edit the tests.**
     - `py/tests/conftest.py:20-24,40,79-102`
     - `py/tests/test_cross_env_is_hermetic.py:28-29,305-1905`
     - `py/tests/test_cross_skip_accounting.py:1360-1361`, plus the four "7 tools" strings
     - `py/tests/test_validation_graph_contract.py:418`
     - `py/tests/test_repo_dispatch_wiring.py`, `test_cli_surface.py:20-25`
     - `py/tests/test_packaging_infrastructure.py:397-421,457-473`
  5. **Remove the differential wiring.**
     - `cross/differential.py:123-124,404,8390-8391,12621-12622,12711-12714`
     - the `cross/README.md` rows
     - `validation/cross.dag.yaml:130-154`
  6. **Update validation:** the agentctl entry in `validation/components.json` (Rust prefixes, `test_files: []`); delete `validation/python-components.dag.yaml:13-35` and `validation/packages.dag.yaml:7-19`.
  7. **Update scripts.**
     - `scripts/check_python_packages.py:157-170,203,528,1069-1086`
     - `scripts/check_deps.py:30-33`
     - `scripts/check_rust_packages.py:64-68,312-314`
     - `scripts/embed_userguides.py`
     - `scripts/check_documented_defaults.py`
     - the `py/README.md:15,27` row
  8. Run `make validate-all` bare, then `make check`. Rebase and push, or use the one exceptional PR with `Exception-Reason: high-risk-preland-review` (Q22).
  9. Close `agentctl-rust-only` with what remains unverified: hosts other than host-a and host-b, live Google Chat, a real box run. Notify consumer-a.
- **Collision:** both core teams must be quiet on `py/agentctl` for this one commit.
- **Acceptance:** no tracked file outside `archived/` imports Python agentctl; `make validate-all` is green; the issue is closed.

---

## 4. Policy while porting

1. **Rust first.**
   - Every new agentctl capability, option, output field or behaviour change lands in Rust, in the same commit as the Python change or before it.
   - Until the M7 switch it is also mirrored into Python, because both editions have users.
   - After M7, Python receives only regression or security fixes until the M8 freeze, and nothing after it.
   - A Python-only feature commit is refused.
2. **Python-surface gate (from M0).**
   - `scripts/dump_python_agentctl_surface.py` dumps the argparse trees of `agentctl`, `agentctl chat`, `herdr-agent`, `herdr-chat` and `herdr-subagents`, plus both MCP tool schemas, to `rs/agentctl/tests/fixtures/python-surface.json`.
   - A Rust test fails if any entry lacks a Rust counterpart, or a row in `surface-mapping.json` naming a milestone or a §1.2 difference.
   - `make validate` regenerates and diffs the dump, so an unmapped Python addition fails validation.
   - The generator runs Python only at test time and is deleted in M9; the fixture stays frozen.
3. **Differentials stay until replaced.**
   - `cross/agentctl_differential.py` and `cross/herdr_agent_differential.py` remain in `make validate` (`validation/cross.dag.yaml:130-154`).
   - A group is deleted only when the Rust contract suite has an equivalent case with a recorded golden. A Rust test asserts that every Python `build_report` group name is either ported or listed.
   - Until the freeze, new scenarios go into both the Python harness and the Rust contract suite.
4. **New differentials for new surfaces:** worker-rpc (M3f), headless lifecycle and migration (M3), mcp (M4), herdr-subagents output (M5), and the chat translator and status (M6). All are removed in M9.
5. **Hot-file etiquette** follows the collision rules in §3.0. Hook commits are announced and kept small. Logic goes in new files. The chat agent's files are requested, not edited.
6. **Differences are recorded.** Any behaviour that knowingly diverges needs a §1.2 row and a userguide entry before it lands.
7. **Validation:** run `make validate` bare, with no pipe, before every push. Edits to cargo-runner, `common/bin` or the resolver need `make validate-all`.

---

## 5. Risks and open questions

### Risks

- **R1 Moving target.** Both core teams change both editions daily (19 mirrored core commits since 2026-10-08). Mitigation: the Rust-first policy, the python-surface gate and the M8 freeze.
- **R2 Hot-file conflicts.** `subagents.rs` (21 commits a week), `cli.rs` (13), `USER_GUIDE.md` (20) and the chat files (23). Mitigation: hook commits only.
- **R3 Chat-owner dependency.** The M6g hooks may slip, leaving D9, D10, D12 and CHAT-14 interim indefinitely.
- **R4 First HTTP/TLS stack.** It brings supply-chain surface, ring's C build and offline prefetch. Container crate fetches already fail on this host.
- **R5 Silent key reordering.** `serde_json` `preserve_order` unification (`rs/tick-hub/Cargo.toml:18`) already threatens herdr-agent's sorted output (`legacy_cli.rs:536-544`). Mitigation: sort explicitly, plus a golden built under a workspace build.
- **R6 Switch prerequisites.** Through cargo-runner, the Rust default breaks coordinators inside boxes (`rs/bin/cargo-runner:106-110`; `py/wrkslots/sandbox.py:253-264`) and adds 0.3–2.2 s per call. The M7 fast path is a hard prerequisite of the switch.
- **R7 Mixed-edition runtime homes.** During the transition, two runners and two CLIs may share one home. Mitigation: byte-compatible files, flock interop and cross tests.
- **R8 Snapshot GC** could delete a live runner's or chat service's executable.
- **R9 Unverified Google Chat API assumptions** (`messageId` prefix, `requestId` interplay, filter syntax). Verify against the live API on host-b before M6 acceptance.
- **R10 Port clash.** The WebSocket default port 18765 collides with vibe-talk's example bridge config (`vibe-talk/README.md:1404`, `vibe-talk/src/config.rs:2459`).
- **R11 Survey coverage.** Only host-a has been surveyed (239 records, no turn-runner or v2/v3 rows). Other hosts may hold legacy state.
- **R12 Internal default in public help.** Rust's help embeds an internal agentcloud endpoint as a default (`rs/agentctl/src/subagents/cloud.rs:373`), and no linter catches it. This matters if any crate is published.
- **R13 Name collisions:** an unrelated `agentctl` 0.1.0 is on crates.io, and an unrelated `agentctl` is on PyPI.
- **R14 Coverage loss** across about 1,174 Python test functions and 30 groups. Mitigation: the M8 test map.

### Open questions (each with a recommendation)

- **Q1 Exit codes.** Recommend D1: exit 2 for refusals that can never succeed; keep 75 and 69 for headless runtime failures.
- **Q2 JSON format.** Recommend matching the compact pending line and sorted keys, and keeping raw UTF-8 (D2).
- **Q3 capabilities shape.** Recommend D3: Rust's object-valued services, Python's exact `headless` object, and an `mcp` service object.
- **Q4 Python defects.** Recommend fixing all of D14 a–k and documenting each fix, rather than reproducing them.
- **Q5 Policy hook.** Recommend the executable `agentctl-subagents-policy/v1` protocol (D4), not a `python3` shell-out for user policies.
- **Q6 MCP surfaces.** Recommend the exact 18 and 9 tools in v1, with `rename`/`anchor`/`doctor`/`revive`/`profiles`/`capabilities` tools added later as a superset. Should `muse` be added to the `subagent_up` harness enum? Recommend yes.
- **Q7 Supersets.** Approve `migrate --replace-runner` (needed for moving live Python runners to Rust without loss), `herdr-subagents repair`, and the `quickstart`/`userguide` subcommands.
- **Q8 Nested v2/v3.** Recommend full read and nested write-back ("nothing dropped") over read-and-migrate.
- **Q9 Codex goal.** Recommend a documented `agentctl codex-goal get|set|clear`.
- **Q10 Google Chat packaging.** Recommend a separate crate (`rs/chat-subscription-google-chat`), with agentctl linking only its `config` feature, and blocking `ureq` + `rustls` (ring). Which roots: webpki-roots, native certificates, or both? Native certificates are needed behind TLS-inspecting proxies.
- **Q11 Google Chat behaviour.**
  - Failure backoff: keep it in the plugin (state under `XDG_RUNTIME_DIR`) or change the host?
  - Keep the 3600 s default poll interval? Recommend yes, for parity.
  - Own-reply suppression via `messageId` prefix (D13), pending API verification?
- **Q12 Python chat state.** Recommend re-admitting open requests by default, and refusing unsent replies unless `--abandon-open` is given. Should migrate instead offer to re-send them via `chat publish`?
- **Q13 `--bridge-state`.** Recommend making it optional, defaulting to `<registry>/.chat` as in Python.
- **Q14 Chat hooks.** Who implements M6g-1…6, and when? Proposed: the chat-service owner, on their schedule.
- **Q15 Freeze date and hook commits.** Proposed:
  - Rust-first starts at M0; mirroring continues until M7; the Python freeze is at M8 start.
  - The interactive team accepts one M3d hook commit in `subagents.rs` and `cli.rs`.
- **Q16 herdr-subagents packaging.** Recommend a third `[[bin]]` in the agentctl crate.
- **Q17 Python-free tests.** Should "no Python" also apply at test time? Recommend yes, with the python-surface generator as the one exception until M9.
- **Q18 Soak shim.** Allow an env-gated `AGENTCTL_WORKER_RPC` in `py/agentctl/sessions.py:39`, so Rust runners can soak under the Python CLI? Recommend no unless soak time is short.
- **Q19 Containment.** Recommend `PDEATHSIG` in the first port, and the cgroup leaf as opt-in `auto`.
- **Q20 Escape hatch after the archive.** Recommend only the frozen tag through `git worktree`; no unvalidated `archived/` dispatch. How long is the sunset?
- **Q21 Distribution.**
  - Is the snapshot fast path enough, or are CI-built release artifacts wanted?
  - Is cargo a hard requirement for a fresh harness? The setup skill currently promises no build step (`skills/agent-utils-setup/SKILL.md:188-191`).
  - Stale snapshot in a read-only box: recommend refusing.
- **Q22 Archive day.** Use the single exceptional PR (`high-risk-preland-review`), or go directly to main?
- **Q23 Goldens.** Is Python's output, where the editions agree, acceptable as the permanent contract, including exact stderr text?
- **Q24 Tracker.** File `agentctl-rust-only` in `.minibeads` or on GitHub?
- **Q25 WebSocket details.** Keep the 18765 default with fail-and-log (recommended), or implement the auto-increment that `server.py:26-27` claims but never had? Use the `sha1` crate or hand-roll SHA-1?
- **Q26 Owners for host checks.** Who runs the host-b survey and the one live Google Chat verification?