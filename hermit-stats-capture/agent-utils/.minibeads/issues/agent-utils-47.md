---
title: 'read-aloud-overlong: a full read stalls about 75 s mid-response and runs far past its spoken length'
status: closed
priority: 2
issue_type: bug
assignee: opus-5.5/read-aloud-overlong
labels:
- vibe-talk
- audio
depends_on:
  agent-utils-37: discovered-from
created_at: 2026-09-26T09:35:28.961358042+00:00
updated_at: 2026-10-13T09:47:58.679697370+00:00
closed_at: 2026-10-13T09:47:58.679697250+00:00
claimed_at: 2026-09-26T09:35:28.963597195+00:00
claimed_until: 2026-09-28T09:35:28.963455011+00:00
---

# Description

[opus 5.5] Split out of #37 read-aloud-cut-truncation, which closed on its acceptance. That issue was about the wrong message, or only a few words, being read after a cut. This one is about full reads that run far too long.

Symptom: a full read of a message of about 1,000 characters produces 140-180 s of audio. The same message read after a cut takes about 68 s. The listener hears the read drag on.

Evidence, content-free, from the deployed internal provider bridge's logs on 2026-09-25 and 2026-09-26:
- In each overlong response, the voice provider keeps streaming audio frames at real-time pace. Meanwhile no transcript text arrives for 61-77 s, and most often for 75-76 s. Then text resumes and the read finishes.
  - The total transcript stays at about the message length (989-1,055 characters), so the message is not repeated.
  - The response then ends normally. It is not a turn that never ends.
- The stall often begins 3-4 s into the response. Its length is nearly constant, which points to a timer upstream of the bridge. The provider's decoder kept a steady output rate through the stall.
- It predates the #37 read-aloud-cut-truncation fix:
  - It hit 9 of 29 responses of 30 s or more on at least four earlier builds.
  - Those reads ended at 119.4 s, about halfway through the message. The bridge's former 120 s whole-response limit cut them off without reporting it.
- Since that limit became an idle timeout, it has hit 5 of 8 such responses, and they now run to their natural end: 145 s, 165 s, and 179 s interrupted.
- Not yet known:
  - Whether the audio during the stall is silence or speech that has no transcript.
  - Whether the rate is the same on a build without the fix.

Why this repository cares: a read that stalls for over a minute looks broken. The page keeps a completed read's message archived only once the read ends.

Owner: opus-5.5, the agent that delivered the #37 read-aloud-cut-truncation fix. Plan:
1. From the provider's per-response token and audio-chunk timing, decide whether the text stream stalls or the speech synthesis emits filler. Measure audio energy during the stall.
2. If the cause is in the bridge, fix it there with a regression test.
3. If the cause is in the provider, hand it the timings. This issue stays open until the acceptance test passes live.

# Acceptance Criteria

Run live on the deployed stack and report content-free:
- 10 fresh-session full reads of long messages, about 1,000 characters each.
- No response has more than 15 s where audio flows but no transcript arrives.
- Every read ends with a completed response.
- Audio seconds per transcript character stays at or below 0.09. The post-cut reads measured 0.068 and the stalled control 0.157.
- The #37 read-aloud-cut-truncation re-answer probe still reports 0 of 2.

# Notes

[gpt-5] A provider-side fix is prepared, rebased onto current source and preserved in a verified
recovery bundle. Review found one additional cached-context edge case: the teacher-forced opening
token was counted as heard even when fewer than the measured 80 audio tokens followed it. The fix
puts that token through the same voice-lag accounting as generated text, so a budget cut re-says
an opening word whose audio never arrived.

Validation on the final local successor:

- runaway-guard target: 30 passed, 0 failed;
- companion session-setup target: 22 passed, 0 failed;
- changed-target type check: five targets, no errors;
- lint: no issues;
- mutation: restoring the old accounting drops the opening word and fails only the new regression.

That private state has since advanced. The per-event bridge timeout is published as D121901941
version `441024423`; the guarded TTS retry is published as D121950561 version `441010047`;
and the exact-read path is published as D123105386 version `441014855` on top of it. The current
versions address the applicable reviewer findings: a hard lifetime beats buffered data after both
deadlines; TTS closes budget-capped interactions before retry and drops bad prosody context even
when retry streaming is cancelled; and exact reads fail closed on stale, malformed, or blank input
behind an independent default-off production kill switch. All three have reviewers, no failed CI
signal at publication, and remain in peer review.

[gpt-5] 2026-10-03 acceptance update: the privacy-contained audio verifier now uses the owned
transcription-only ASR service, with rewriting disabled, instead of replaying through the
conversational agent. A non-exact diagnostic WAV produced one authoritative final segment, 134
recognized words, 0.910 ordered precision, and zero excess 8-grams. Its 0.782 source coverage
correctly fails the exact-read gate, so this is harness evidence rather than acceptance evidence.
The private log was empty and the transient PrivateTmp unit left no process or host temporary
artifact.

The deployed bridge is intentionally unauthenticated because the VM credential is stale. The
supported refresh requires opening the default VM once in normal hardware-bound Hatch Web; no
headless or CLI refresh exists. After that refresh, the issue still requires one exact smoke, 10
fresh default-provider reads with WAV capture and per-WAV ASR coverage and precision of at least
0.85, and the 0-of-2 re-answer probe before it closes.

[gpt-5] 2026-10-03 review and deployment update: the latest exact-read Kepler build
`a52aa3d18f74b5672a603ef9ce1dbf21fcf1a2cd` built successfully and is running from a clean
review checkout with `KEPLER_ENV=dev` and the dev-only direct-read override enabled. The shim,
Oxide, and browser bridge remain on the preserved deployment stack
`8c6927b968eff9245188765edd89ac783f90a9d7`, which carries the devserver-only rate-limit
warmup bypass and ASR-finality support. All four user units are active with zero restarts and all
expected listeners are present. The exact-read/config suites report 104 passes, no failures, and
one pre-existing skip; the full TTS audio suite reports 325 passes; and the corrected Oxide timeout
suites report 296 passes plus one unrelated H.265 skip. The hardware-bound Hatch refresh remains
the only prerequisite before the fresh live acceptance matrix can start.

[opus 5.5] 2026-10-03 consolidation and review-finding update. This supersedes the deployment and review state in the two notes above.

- Deployment: all four voice units now run from one retained review checkout at `e1ad0a10593150552701bcc107543cfedbe83de5` (the deployment-only skip-warmup copy of D123155424 on top of the D123105386 stack). All are active with zero restarts and expected listeners present. Redundant checkouts were removed; local bookmarks preserve `e1ad0a105931` and a local Slack shim note at `09b4e96f4a01`.
- Review: reviewer requests were withdrawn from D123105386 and the rest of the stack. Nothing is in active review, nothing new was published, and no human has accepted any of it. Nothing upstream changes until the owner inspects the evidence.
- The three remaining D123105386 review findings were reproduced before any fix (4 of 4 evidence tests, no warnings):
  1. The denied direct response was announced but never got a terminal: `response.created`, then the replacement, with no `response.completed`. The legacy safety route has the same gap, so this is not a D123105386 regression.
  2. The legacy detached safety response's teardown, which D123105386 extended, cleared a newer generator, forced IDLE, and ended the group when a newer response had been installed without cancelling it. The ordinary barge path was unaffected.
  3. The exact-read safety preconditions were asserts. Compiled at `-O` both vanish and safety receives `text=None`. Not reachable today: the invariant holds by construction and the deployed process runs without `-O`.
- Local fix commit `2167ebafa3d6b84a383d3a460ec5b61475049a46` on top of `e1ad0a105931`, not uploaded: a cancelled terminal (with no unsafe text) before the replacement and exactly one terminal on a failed install; an ownership-guarded legacy teardown; an explicit fail-closed check that holds at both optimize levels. Validation: focused session tests 22/22, safety handler 6/6, verbatim response tests 18/18, response lifecycle 49/49, all without warnings; type check clean on four targets; lint shows only pre-existing complexity advice. The running services were not restarted, so the deployed code does not yet include these fixes.
- Acceptance is unchanged and still pending the stale Hatch token: one exact smoke, 10 fresh WAV/ASR reads, and the 0-of-2 re-answer probe. This issue stays open for them.
