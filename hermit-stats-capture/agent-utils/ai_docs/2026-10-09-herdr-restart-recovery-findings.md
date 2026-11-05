# Recovering agents after a Herdr server restart: what agentctl and wrkslots support, and what they should

Date: 2026-10-09. Author: a consuming project's coordinator (Claude), after a live recovery. Project and slot names below are redacted to `consumer-a` and `<slot>`.

## Background

A coordinator runs persistent coding agents (Claude, Codex, agentcloud, Muse) in Herdr panes, one tab per agent, through `agentctl`. Each agent's work happens in a `wrkslots` worktree slot. The agent's conversation (its *native session*) lives in the harness's own store: for Claude, one transcript file per conversation under `~/.claude/projects/<escaped starting directory>/<session-uuid>.jsonl`, resumable with `claude --resume <uuid>` from the same starting directory.

On 2026-10-09 the owner ran `herdr update` (to herdr 0.9.3) without `--handoff`. The Herdr server restarted and every harness process in every pane was killed. Herdr then restored the tab layout, but each tab came back as a plain shell, without the agent, without the tab label, and in the pane's last directory.

At the time three subagents were mid-task (`liteinst`, `scorecard`, `pause-signal`), plus the coordinator itself and the Google Chat bridge service that delivers owner messages into the coordinator's pane.

## What recovery took

All three agents and the bridge were recovered in about 25 minutes, with their conversations intact. Almost every step went around a gap in the tools:

| Step | What the tools gave | What had to be done by hand |
|---|---|---|
| 1. Which agents were live | `herdr agent list` showed only the coordinator; `herdr pane list` showed ~70 restored shells across 6 workspaces with no labels. `agentctl list` showed 4 records (`liteinst`, `pause-signal`, and two Codex lanes whose harness had already exited the day before). | One live agent (`scorecard`) had never been registered with agentctl at all. Its existence came from the coordinator's own notes. |
| 2. Which conversation each agent was in | **Nothing.** Every record has `session_value: null`, `session_agent: null`, `resume: null`. | Listed the 25 newest transcripts in `~/.claude/projects/-…-consumer-a/`, read each one's first user message ("You are pause-signal, …", "Resume the LiteInst fat lane…"), and for the two that started with `/goal`, grepped their tails for the slot name. |
| 3. Retire the dead record | `agentctl stop pause-signal` exited **75 with no message at all**. | Read `stop --help`, found `--expected-token` ("mandatory for managed-dead recovery"), read the token out of `.agentctl/pause-signal/agent.json`, reran. That worked and closed the stale tab. |
| 4. Retire an adopted record | `agentctl stop liteinst` refused: "recorded pane shell generation changed". No flag covers it (`--recover-legacy-adoption` is only for identity-less rows). | Moved `.agentctl/liteinst` into `.agentctl/archive/` by hand. |
| 5. Relaunch with the same launch policy | `agentctl start NAME --profile claude-opus-55 --resume UUID` refused: "--profile conflicts with explicit launch settings: --resume". | Read the profile (`harness claude`, `model opus`, no extra arguments) and passed `--harness claude --model opus --resume UUID` explicitly. The record no longer names the profile. |
| 6. Tell each agent it was restarted | The first note was delivered; the second and third (same text, sent to other agents) were **quarantined as PROBABLE MISROUTE**, because the identical note was already visible in a peer's pane. Changing only the first line did not help: the read-back compares the prompt's tail. | Checked each pane by hand: every note had in fact arrived exactly once, with no copy in any other pane. |
| 7. Chat bridge | `systemctl --user is-active` said `active`, and `chat status` said the provider was healthy. But the service had started the previous day and logged nothing since the Herdr restart; its event subscription to the old Herdr server was gone. | Restarted the service by hand. |
| 8. Leftover tabs | The restored shells (old agent tabs, closed lanes, herdr-run panes, validate panes) are still there. | Not cleaned up; no tool knows which restored shells belonged to which record. |

## Findings

1. **agentctl does not record the native conversation ID for Claude agents.** This is the single biggest gap. Without it, resuming means reading transcripts by hand. The 2026-10-08 design (`ai_docs/transient/2026-10-08-agentctl-rename-identity-revive-design.md`) already found this ("0 of 37 archived Claude records" carry a session) and proposed assigning `--session-id <uuid>` at start and storing it in a new `conversation` field. Stage 1 landed rename, identity anchors, doctor and misroute handling, but not that.
2. **There is no resume-by-name command.** `start --resume SESSION` exists, but the caller must know the session ID, must give the name again, and cannot keep the profile (finding 5). There is no `agentctl revive NAME` and no "restore everything that was running" operation.
3. **Retiring a dead record is undiscoverable.** A managed record whose harness died needs `stop --expected-token TOKEN`, and the token is only in the private `agent.json`. Without it, `stop` exits 75 silently. (A refusal with no message is itself a defect.)
4. **Adopted records cannot be recovered after a Herdr restart.** Herdr restarts every shell, so the recorded shell generation always changes and `stop` refuses forever.
5. **`--profile` cannot be combined with `--resume`.** Resuming therefore drops the owner's launch policy or forces the caller to copy it by hand, which the agentctl skill forbids in normal use ("never invent, add, remove, or rewrite harness arguments").
6. **The record does not keep the profile name or the slot.** After a restart nothing says which profile, model or slot an agent ran with. `wrkslots` records the agent name per slot, but not the session, and agentctl records no slot (agents often keep their starting directory at the project root and work in the slot by absolute path, which is also the directory Claude needs for `--resume`).
7. **Unregistered agents are invisible.** An agent started outside agentctl (or registered under a different registry) leaves no record to recover from.
8. **Identical restart notes trip the misroute quarantine** (known Gap 3 of https://github.com/rrnewton/agent-utils/issues/231). The obvious recovery action, sending the same "you were restarted" note to every agent, fails safe for every agent after the first.
9. **The chat bridge does not notice a Herdr server restart.** It stays `active` and reports a healthy provider while it can no longer deliver.
10. **`herdr update` without `--handoff` kills every agent.** `herdr update --handoff` ("try live handoff after installing") exists; nothing in agent-utils warns about this, checks for live agents before an update, or snapshots what is running.
11. **`start --resume` cannot resume into the existing pane.** `start` always opens a new tab. `stop` closed the stale tab for the managed record, but the adopted and unregistered agents left their restored shell tabs behind, so the owner briefly saw two tabs named `liteinst` (and two named `scorecard`) and the old ones had to be closed by hand with `herdr tab close`.
12. **agentctl breaks when the Herdr client and server disagree, and does not say so up front.** Mid-recovery the `herdr` on `PATH` was a 0.8.0 client (protocol 19) while the server was 0.9.3 (protocol 22). Every pane query failed with `protocol_mismatch`; the chat bridge crash-looped into systemd's start limit. Neither `agentctl` nor the bridge checks `herdr status` compatibility at startup or reports it in `doctor`/`chat status`.
13. **With Herdr 0.9.3, agentctl's readiness and process checks fail for Codex and Muse.** Herdr reports a Codex pane's `agent_status` as `unknown`, so `start` timed out ("timed out waiting for agent startup") although Codex was running and idle, and `drain` timed out the same way; the brief had to be typed with `herdr agent prompt`. For a Muse launch, `pane process-info` returned no foreground argv and agentctl refused the response ("expected an array, got NoneType"). `adopt` refuses Muse outright, so a Muse agent started outside `start` stays unregistered.
14. **A profile can name a reasoning effort the model rejects.** The owner's Muse command used `--reasoning-effort max`; the model `kiki_gb300_mxfp8_6p2_840_nwr` returns HTTP 400 ("supported values: minimal, low, medium, high, xhigh"). `agentctl profiles` could validate effort against the harness's model catalogue before a launch.
15. **wrkslots cannot rebind a slot to the agent's new session.** Every slot that existed before the restart records an owner PID that is now dead. `wrkslots adopt` from the resumed agent refuses ("already has a dead historical owner ... ordinary adopt cannot replace it"), and `recover-unbound-owner` only records coordinator evidence; by design it never replaces the owner. The slots keep working only because agents had put them on hold.
16. **who-am-i cannot name an adopted Codex agent.** The resumed liteinst lane (Codex, adopted because `start` timed out) tagged its commit `[<slot>, unknown-agentname, gpt-6-astra, role=impl]`. Agent-name resolution depends on a record shape that adoption after a launch failure does not produce. who-am-i also does not recognise Muse at all (tracked separately).

## Suggested improvements

In priority order.

1. **Record the conversation ID at start and show it.** For Claude, generate a UUID and pass `--session-id`; for Codex and Muse, capture the native session the harness reports; for agentcloud, the session ID is already recorded. Store it with the profile name, model, starting directory and (when known) slot. Show it in `list` and `status`.
2. **Add `agentctl revive NAME` and `agentctl revive --all`.** For each record whose lifecycle is `running` but whose harness is gone: open a fresh tab, launch with the recorded profile, directory and `--resume <conversation>`, retire the dead record and its stale tab, and optionally send a note. `--all --dry-run` prints the plan, which also answers "what was running?".
3. **Make dead-record retirement one obvious step.** When `stop` refuses, always print why and the exact recovery command (including the expected token). `revive` should do this itself.
4. **Give adopted records a recovery path after a server restart** (for example, recognise that the whole Herdr server restarted, and allow retire-without-close when the recorded harness process no longer exists).
5. **Allow `--profile` with `--resume`.** Resuming should keep the owner's launch policy.
6. **Snapshot before a Herdr update.** A small `agentctl snapshot` (or a pre-update hook) writing name, profile, directory, slot, conversation ID and pane for every live agent, so that recovery never depends on transcript archaeology. Recommend `herdr update --handoff` in the agentctl user guide and quickstart.
7. **Let `wrkslots status` show the session per slot** (agent name, harness, conversation ID), or point to the agentctl record.
8. **Fix Gap 3 for recovery notes**: have agentctl prefix each sent note with a unique per-message nonce line (and compare the whole prompt, not only its tail), so identical broadcasts can be attributed.
9. **Bridge liveness**: exit non-zero when the Herdr event subscription ends (so systemd restarts it), and add a delivery health check that `chat status` reports.
10. **`doctor`**: flag "lifecycle running, harness gone" as `revivable` and print the `revive` command; flag restored shells in the configured workspace that belong to no record.
11. **`revive` should reuse or close the old tab**: either relaunch inside the record's existing (restored) pane, or close it once the new tab is up, so a name never has two tabs.
12. **Check Herdr compatibility first**: `agentctl`, `doctor` and the chat bridge should run the equivalent of `herdr status` at startup and fail with one clear message naming the client and server versions and protocols, instead of failing every operation later.
13. **Herdr 0.9.3 compatibility**: treat a Codex `agent_status` of `unknown` with an idle composer as ready (or detect readiness from the composer), accept a missing foreground argv as "unknown", and allow `adopt` for Muse with the same identity checks as other harnesses.
14. **Validate profiles**: reject a profile whose reasoning effort or model the harness does not accept, at `profiles` time.
15. **Let a resumed agent re-own its slot**: a `wrkslots rebind` (or an `adopt --replace-dead-owner`) that requires proof the old owner process is gone and the new process runs the same conversation (agentctl's recorded conversation ID).
16. **who-am-i should take the agent name from the agentctl record for every adapter** (managed, adopted, launch-failed-then-adopted), and recognise every harness agentctl can launch.

## Status

Updated 2026-10-09. The sections above describe the restart; the numbers below
refer to **Suggested improvements**. These changes are landed on agent-utils
`main`:

| Suggestions | Delivered | Commit |
|---|---|---|
| 1 | Native conversations and launch metadata for new records, shown in list/status | `645f56d9942b5c0320140eb6c531285f61962a60` |
| 2 | Revive positively dead owned interactive agents and close proved stale tabs | `e9cd2b88669c2a261b1493f5500747c9ab7f8f39` |
| 3 | Explained stop refusals and exact recovery commands | `25b51acce89071901217830e161dc56feb85253e` |
| 4 | Retire a positively dead adopted record while preserving the restarted runtime | `113d67dba2ed3936379fe3d8eeb2dc25ff18762a` |
| 5 | Local interactive `--profile` with native `--resume` | `756f54ea06eb4899e34f9168f7d722c91e7880d8` |
| 13 | Herdr 0.9.3 Codex readiness, missing foreground argv and Muse adoption | `f281f5cd0f386f9f4d555b227bcff0272c15cb3d` |

`stop --retire-dead-adoption` requires the exact token/raw-record digest and a
positive kernel proof that the stored harness generation is dead. It archives
the original record, output and queue/quarantine without runtime or tab effects.

Suggestion **16** is landed in the companion identity tooling as
`6948411d19af4319ba1946bb7ae4811b91609697`: `who-am-i` resolves agent names
from managed and adopted agentctl records, including adoption after a failed
launch, and recognises Muse.

Remaining open:

- **6–7:** pre-update snapshots/handoff guidance and slot/session visibility.
- **8–10:** identical recovery-note attribution, bridge liveness and doctor
  revival/orphan guidance.
- **11:** owned-agent stale-tab closure ships with 2; broader old-pane reuse and
  cleanup for adopted or unregistered agents remain open.
- **12, 14–15:** client/server protocol preflight, profile/model validation and
  conversation-based slot rebinding.
