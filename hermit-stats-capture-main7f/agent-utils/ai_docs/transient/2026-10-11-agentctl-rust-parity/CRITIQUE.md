# Completeness critique of SPEC.md

**Verdict:** the spec is close, but material gaps remain. Below are 15 omitted or misclassified capabilities, 8 order or design problems, and 9 acceptance criteria that cannot be tested as written. Everything was checked against HEAD `fbc8c497`, read-only, and no repository files were changed.

## A. Capabilities omitted or misclassified

**A1. "No Python at runtime" is false for `--slot` and `--project-box`.**
- Rust runs `wrkslots shell-command` for both (`rs/agentctl/src/subagents.rs:3872,3891-3957`).
- wrkslots is Python-only (`common/bin/engine-resolver:4-5`; `py/bin/wrkslots -> ../wrkslots/__main__.py`).
- The DoD-2 guard stubs only "prove" self-containment where tests use a fake wrkslots.
- Fix: either narrow DoD-2 to "agentctl never runs an interpreter itself; wrkslots is an external dependency", or plan a native `shell-command`.
- The setup skill also deploys a Python liveness probe (`skills/agent-utils-setup/SKILL.md:225`).

**A2. Migrating Python chat state (M6f) breaks requests already in flight.**
- Python requests can be in `awaiting_reply`, `delivery_uncertain` or `reply_pending` (`py/agentctl/chat.py:3481-3491,3884-3900`). Re-admitting them by default sends the live coordinator a prompt it already has. That breaks the never-replay rule.
- The coordinator's context holds Python keys and nonces, and a command `python3 …/chat.py reply --state … --request KEY` (`chat.py:2331,2515`). Rust will not recognise any of them, and after M9 that path no longer exists.
- Python `chat launch` stops the coordinator when its bridge exits (`chat.py:5226,5236`). Releasing `.run.lock` so that migrate can run therefore kills a coordinator hosted by `launch`.
- Fix: refuse to migrate unless no request is in one of those phases (or import key and nonce aliases), and document a `chat launch --resume SESSION` relaunch path.

**A3. Chat outputs and exit codes differ, and none of it is in §2.5 or §1.2.**
- Python output:
  - `init` prints `{state, space}` (`chat.py:5458`).
  - `reply` prints `{outcome:"reply_queued"}` (`chat.py:5461`).
  - `close` prints `{outcome:"reply_capture_closed"}` (`chat.py:5464`).
  - `status` and `tick` print the bridge status document (`chat.py:5485,5495`).
- Python exit codes: every error exits 1, and Ctrl-C exits 130 (`chat.py:5508,5512`).
- Rust behaves differently:
  - `tick` exits 75 when it hits errors (`rs/agentctl/src/cli.rs:1638-1646`).
  - `reply` exits 75 or 2.
  - `run` prints a JSON result.
  - All of the JSON shapes differ.

**A4. `--reconcile-interval` has the same name but a different meaning and range.**
- Python: a REST recovery scan, 10–86400 s (`chat.py:5363,5500`).
- Rust: disk-backed snapshots, any positive value (`rs/agentctl/src/cli.rs:799-801`).
- CHAT-09 lists only `--interval` and `--observer-write-interval`.

**A5. `chat launch` has options that CHAT-07 and M6c never list.**
- Missing: `--harness codex|claude`, `--resume`, `--harness-arg`, `--agent-label`, `--after`, `--interval`, `--reconcile-interval`, `--observer-write-interval` (`chat.py:5337-5358`).
- Also missing: `--model` doubles as the default reply label (`chat.py:4632-4637`).

**A6. The §4.2 gate only dumps argparse trees and MCP tool schemas.** DoD-1 also promises "schema field" coverage, but nothing generates it. Not dumped:
- **Environment variables (about 30):** `foreign/lib.py:32-117,136,2238`; `foreign/mcp/server.py:25-28`; `HERDR_CHAT_TOKEN` (`chat.py:661`); `AGENTCTL_MISROUTE_COUNTERMAND` (`subagents.py:1035`).
- **Chat configuration and target fields:** `chat.py:653-669`.
- **Python edition's own `codex_goal` parser:** `codex_goal.py:294-306`.
- **worker_rpc actions:** `worker_rpc.py:48-81`.
- **Adapter actions** poll, send, react, context and subscribe: `chat.py:1560-1588`; `chat_runtime.py:244`; `chat_context.py:105`.
- **Policy hooks** `backend()` and `check_launch(harness, model, purpose)`: `foreign/policy.py:12-20`.
- **MCP methods** (`ping`, `notifications/*`) and the herdr-subagents result shape (`structuredContent`, `ok:true`): `mcp.py:130`; `server.py:406,417,445`; `adapter.py:45`.
- **Generator caveat:** the herdr-subagents parsers are built inside each `main()` (for example `foreign/agent_up.py:40-53`), so dumping them means intercepting `parse_args`.

**A7. D1's exit-code list is incomplete.**
- Every `AgentDeliveryError` exits 75 (`errors.py:51-53`).
- M3 will port about ten more refusals based on argument meaning (`sessions.py`):
  - an invalid profile name (`:89-90`);
  - headless with `--resume`, `--workspace-id` or `--env` (`:108-111`);
  - harness arguments or effort on a non-Muse harness (`:117-122`);
  - `--message-id` on a headless worker (`:227-228`);
  - `--since-turn` without `--output since_turn` (`:239-242`);
  - adoption-recovery flags on a turn-runner (`:278-285`);
  - reset, repair or migrate on a record that is not a turn-runner (`:317-318`);
  - the wait timeout range (`:326-327`).
- D1 needs a general rule and a test table that covers all of them.

**A8. CHAT-21 breaks a valid Rust command line.**
- Today Rust's `--registry` is global and has `state` as an alias (`rs/agentctl/src/cli.rs:31-39`). So `chat run --state REG --bridge-state B` is valid and means the registry.
- After M6b, `--state` means the bridge state, and passing both is refused.
- consumer-a runs Rust `chat run`, so DoD-8's "nothing it runs changes" fails at its next snapshot bump. This needs a §1.2 row and a check with the consumer.

**A9. Google Chat needs a one-time `agentctl-google-chat install` (CHAT-24).** Python needs no install step (`chat.py:1474-1486`). There is no §1.2 row for this.

**A10. The DoD-8 survey misses several things that would break:**
- `HERDR_SUBAGENTS_POLICY` `.py` files, which D4 now refuses;
- dormant worker homes (`~/.local/state/herdr-agent/foreign/registry.json`, or any `HERDR_SUBAGENTS_HOME`). The survey only uses `pgrep`, which finds live runners only;
- MCP client registrations that point at `py/bin/*`;
- systemd units or cron jobs that call `py/bin/*` or `python -m agentctl`;
- wheels installed with pip;
- running Python `chat launch` deployments and keeper tabs.

**A11. Chat docs are blocked by collision rule 3.**
- CHAT-15 (M6a/M6b) and DoD-5 need docs for `launch`, `context`, `migrate` and Google Chat in the chat guide.
- Rule 3 forbids editing `embedded_chat_*.md`, and M7 hands the merge to the chat owner, so the new subcommands would ship undocumented.
- The new crate's directory, `rs/chat-subscription-google-chat`, also falls inside rule 3's never-edit pattern `rs/chat-subscription*/`.

**A12. The services name changes, not only the shape.** Python reports `"chat"` (`py/agentctl/cli.py:317`); Rust reports `"chat-bridge"` (`rs/agentctl/src/cli.rs:1691-1695`). D3 covers only the object shape.

**A13. D5/D6 do not cover the other Python entry points:**
- `python -m agentctl`, `-m agentctl.chat` (`chat.py:4,5520`), `-m agentctl.foreign` and `-m agentctl.worker_rpc` (`worker_rpc.py:100`);
- running any `foreign/agent_*.py` directly.

**A14. herdr-subagents names allow more than agentctl names.**
- The pattern is `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$` (`foreign/lib.py:122`): mixed case, underscores, up to 64 characters. agentctl names are 1–32 lowercase.
- M3 and M5 never say the worker runtime keeps that grammar.

**A15. Some herdr-subagents options are not listed.** Low impact, but the gate cannot see them:
- `turn` options `--fresh`, `--effort`, `--quiet` and `--tail`, and reading the message from stdin (`agent_turn.py:56,154-171`);
- the MCP-only `subagent_down archive:false` (`server.py:181-187`).

## B. Order and design problems

**B1. M7 fast path.**
- The snapshot fingerprint covers every tracked and untracked file under `rs/` (`common/bin/rs-source-fingerprint:9-13`). Any edit by any team (the chat agent, tick-hub, dagrun) makes the agentctl snapshot stale.
- In a writable checkout, the next `agentctl` call rebuilds under a lock, so coordinators stall.
- In a read-only box it exits 3 (Q21), so coordinators lose agentctl. Python always runs, in 0.13 s.
- Fix: fingerprint only agentctl's own dependencies, and/or make installed pinned snapshots the default for coordinators.
- R6 covers only the box lock problem, not how often snapshots go stale.

**B2. M9 moves files out from under live Python processes.**
- Python starts helpers by file path and imports lazily:
  - the runner wrapper (`foreign/lib.py:51,2244-2257`);
  - the supervisor started for every adapter call (`chat.py:1066-1081`; `chat_input.py:203-212`);
  - the launch gate (`chat.py:4757`) and the bridge (`chat.py:5174`).
- A `git mv` in any checkout or submodule that pulls the archive commit breaks them mid-flight.
- M9 step 1 checks only for commits in flight; it needs a check for live processes and a submodule-update notice.

**B3. M7 waits for M1, M3, M4, M5 and M6 (100–130 agent-days).**
- That keeps §4.1 dual landing on both core teams for the whole port.
- `engines.tsv` is per tool, so the switch can be staged:
  - herdr-agent right after M1 plus the fast path (it is already "same");
  - herdr-subagents after M5;
  - herdr-chat after M6e;
  - agentctl after M6c.

**B4. The gate and Rust-first policy bind the core teams from M0.** Every Python CLI change would need a regenerated `python-surface.json` and a new mapping row. Q15 does not ask for that agreement. The dump also imports `foreign/lib.py`, which reads and parses environment variables at import time (`:32-117`), so the dump must run hermetically.

**B5. Mixed-edition homes.**
- When the Python CLI runs `migrate` or `repair` (re-spawning the runner) or `reset`, the new runner is Python (`foreign/lib.py:2247-2257`). The Rust sidecar `runner.json` can also be lost.
- The spec does not say what Rust does when the sidecar is missing.

**B6. Meta-internal fit.**
- M6b's new helper action `thread_history` is missing from the Iris helper, so `chat context` fails inside Meta unless Iris adds it.
- The M6f translator hard-codes the public polling plugin. It should accept `--plugin`, so it can target an installed alternative such as Iris.

**B7. M8 writes its fixture generator into `archived/agentctl-python/tools/`.** That directory only exists after M9.

**B8. M0 docs symlinks can be silently undone.** An editor that saves by write-then-rename replaces the symlink with a regular file and forks the guide. Add a check that `py/agentctl/{USER_GUIDE,QUICKSTART,README}.md` are still symlinks.

## C. Acceptance criteria that cannot be tested

- **C1. DoD-1:**
  - The gate passes when an entry merely maps to a milestone, so it cannot show completion. It needs a final-state rule that no milestone mappings remain.
  - "Rust counterpart" is never defined; a matching name does not mean matching behaviour, as A4 and A8 show.
- **C2. DoD-2:**
  - Guard stubs on PATH miss absolute interpreter paths (exactly the four `#!/usr/bin/python3` fakes) and external tools written in Python (A1).
  - It needs an exec audit, for example `strace -f -e trace=execve` or seccomp over the contract suite, plus a lint.
- **C3. M3:** "#30 met, or re-filed precisely" can always be satisfied.
- **C4. M6:**
  - "Recorded as unverified until done" is not a criterion, and it contradicts R9 ("verify … before M6 acceptance").
  - D13's own-reply suppression rests on unverified API behaviour. Replies are posted as the owner, so if that assumption is wrong the bridge re-ingests its own replies.
  - Make the live check a hard gate before the M7 switch.
- **C5. M7:** "≤50 ms" names no host, percentile, cache state or staleness rate. The fingerprint alone takes about 43 ms.
- **C6. M2:** "matches the recorded Python goldens" cannot hold for groups that contain D1/D2 cases unless there is an exception list.
- **C7. §4.1:** "A Python-only feature commit is refused" has no enforcement for changes that touch only behaviour.
- **C8. DoD-8:** "every hit is migrated" means little while the survey misses the items in A10.
- **C9. M5:** "matches Python on the cross corpus" defines neither the corpus nor how timestamps, PIDs and paths are normalised.

These spec claims checked out at HEAD:
- the v2/v3 refusal (`subagents.rs:4498-4505`);
- turn-runner refusal (`:1955-1966`);
- headless refused at `cli.rs:1269-1271`;
- the capabilities document (`cli.rs:1675-1730`);
- the four Python fakes;
- the 17-field worker record (`foreign/lib.py:407-430`);
- the D8 key derivation (`chat.py:2331`);
- the D11 bug (`chat.py:4630-4631`);
- the 31+3 test file count;
- no `foreign/` commits since 2026-09-29.