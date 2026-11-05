# voice-unresponsive-signal: say when the voice service stops answering

Status: design reviewed; implemented with the changes listed under "Changes after review".
Author: opus 5.5. Related: #13 voice-chat-tools,
#14 voice-agent-prompt.

## Problem

In a recent live failure, `/voice` stayed green ("live") for a whole call in which no turn
produced any speech or any transcript. Nothing on the page, and nothing in the app log, said
anything was wrong. The owner could not tell a broken service from a slow one or from their own
microphone.

What the failing `vibe-talk-v1` socket actually sent, per turn (from a content-free client record):

| Frame | Per turn |
|---|---|
| binary PCM | **present**: 0.9 s, 5.7 s and 1.0 s of audio across three turns |
| `transcript` (assistant) | exactly one, with **empty** text |
| `turn_complete` | one, normal, not `interrupted` |
| `error` | none |

So a trigger of "zero audio bytes" would have **missed** this failure. The owner heard nothing,
so the PCM was presumably silence; that is not verified (see "What this cannot observe").

## What the protocol lets the page observe

Everything below comes from frames `vibe-talk-v1` already carries (README, "deployment-managed
`vibe-talk-v1` WebSocket"). There is no new frame and no provider knowledge.

| Observable | Where |
|---|---|
| `session_started` and its `greeting` flag | JSON frame |
| each assistant `transcript` and whether its text is empty | JSON frame |
| each `turn_complete`, its `turn` and `interrupted` | JSON frame |
| each binary PCM frame: its length **and its samples** | binary frame |
| `error` frames | JSON frame (already shown red; never logged) |
| when the page itself started a turn: a typed `prompt`, or the greeting after `session_started{greeting:true}` | page state |

## Triggers

A turn is **silent** when its `turn_complete` arrives with `interrupted` not `true`, and during
that turn:

- no assistant `transcript` carried non-empty text; and
- no PCM frame was **audible**, meaning no sample's magnitude reached `AUDIBLE_PEAK` (256 of
  32767, about -42 dBFS). Digital silence is 0 and dither is a few LSB, while speech peaks in
  the thousands. Zero PCM bytes is simply the silent case with no frames.

Audibility is measured on arrival, **before** the `speakerOff` drop. Muting the speaker must not
look like a dead service. In a typed (`chat`) call the page plays no audio, so the text condition
alone decides.

The signal fires on the first of:

1. **`silent_greeting`**: the greeting turn (`session_started{greeting:true}` through its first
   `turn_complete`) is silent. A greeting is by definition supposed to speak, so one turn is
   enough.
2. **`silent_turns`**: two consecutive silent turns. One is not enough: a provider may close an
   empty turn for background noise that its turn detection took for speech, and a server that
   completes a turn per response may finish a tool-call-only response with little or no speech
   before the answer follows. Any turn with audible audio or text resets the count.
3. **`no_reply`**: the page started a turn (the greeting, or a typed `prompt`) and nothing
   arrived within `NO_REPLY_MS` = 15 s: no audible PCM, no assistant text and no
   `turn_complete`. That is the same bound read-aloud already applies to this protocol. For
   spoken turns, turn detection is server-side, so the page does not know when a turn began and
   cannot time one. Those rely on (2).
4. **`error_frame`**: an `error` frame. It is already shown in red; the change is that it is now
   also logged, without its message text.

Each cause is reported at most once per call.

## What this cannot observe

State these plainly in the README:

- **Why.** Which component failed is outside the protocol. The signal says "not responding" and
  nothing more, by design.
- **Inaudible but non-silent audio.** PCM that clears the threshold but is noise, or speech too
  quiet to hear, counts as audible.
- **The listener's own output path.** The page judges samples, not speakers. An OS-muted device
  or a disconnected headset looks healthy.
- **Whether the user's speech reached the service.** An empty user transcript does not
  distinguish "recognition is down" from "the user said nothing". Spoken turns the service never
  detects produce no frames at all, so they are indistinguishable from the user staying quiet.
  Only (3) bounds typed turns and the greeting.
- **Whether the silent PCM in the recent failure was all-zero.** That can be confirmed only by
  capturing a failing call. This design makes no more live calls, and the reported `peak`
  (below) answers the question the next time the failure happens.
- **The hosted vendor's protocol** (the default conversation backend). It has its own error
  frames. This design covers `vibe-talk-v1` only.

## UI

A new status state, **`unresponsive`**, set on the first trigger while the socket is open:

- Dot: the paused colour with a warning-coloured ring, no pulse. The CSS sits beside `live`, `error` and `suspended`.
- Sticky status text that does **not** auto-dismiss at 6 s: "The voice service is not
  responding." Hang up stays available; nothing is torn down, because the service may come back.
- Recovery: the next turn with audible audio or text restores `live`. The status then says "The
  voice service is responding again." and dismisses normally.
- `error_frame` keeps today's red `error` state and panel. It only gains the log line.

Why not reuse `error`: `error` says the call is broken and auto-clears its panel after 12 s.
Here the call is open, may recover, and the warning must persist while the condition holds, which
is exactly the gap that left the page green.

## App log

New route `POST /api/v1/voice-health`. It uses write scope like `voice-timing`, is logged and
not stored, and takes a closed body where an unknown field is refused with 400 `invalid_health`:

```json
{"protocol":"vibe-talk-v1","chat":false,"cause":"silent_turns",
 "since_open_ms":21400,"turns":3,"silent_turns":2,"audio_ms":1000,"peak":0}
```

- `cause` is one of the four above, or `recovered`; anything else is refused.
- `audio_ms` and `peak` describe the last silent turn: milliseconds of PCM received, and its
  largest sample magnitude. Together they tell "no audio" (`audio_ms=0`) apart from "silent
  audio" (`audio_ms>0 peak<256`) without a live capture.
- `turns` is the count of completed turns in the call: an ordinal, not an id.
- Every field is required. Bounds are validated like `voice-timing` (`since_open_ms` ≤ a day,
  `audio_ms` ≤ an hour, `peak` ≤ 32768, `silent_turns` ≤ `turns`).
- The body is capped at 512 bytes (413), and the route writes at most 30 lines a minute across
  all callers (then 429 `voice_health_throttled`).

It logs one WARN line and nothing else:

```
voice_unresponsive protocol=vibe-talk-v1 chat=false cause=silent_turns since_open_ms=21400 turns=3 silent_turns=2 audio_ms=1000 peak=0
```

Recovery posts `cause:"recovered"` and logs at INFO as `voice_recovered …`, so a log reader sees
the episode end. There are no words, no session or conversation id, no provider name and no
error message text.

## Tests (each fails without the change)

Page (`tests/js/voice_page.test.mjs`, existing wire fake and fake timers):

- Two turns of all-zero PCM, each with an empty assistant transcript and `turn_complete`:
  `data-state="unresponsive"`, sticky status, one `voice-health` POST with
  `cause=silent_turns audio_ms>0 peak=0`.
- The same with no PCM at all: `audio_ms=0`.
- A silent greeting: fires `silent_greeting` after one turn.
- Negatives: an audible turn, a text-only turn, an interrupted turn, a single silent non-greeting
  turn, and speaker off with audible PCM. None of them fires.
- `no_reply`: greeting expected and 15 s of fake time with nothing arriving; a typed prompt
  likewise; neither fires at 14.9 s.
- Recovery: a silent pair, then an audible turn, gives `live` and a `recovered` POST.
- `error_frame`: POST with the cause only; the body holds no message text.
- Once per cause per call.

Server (`tests/voice_agent.rs`, `tests/logging.rs`):

- A valid record gives 204 and exactly one `voice_unresponsive` WARN line with those fields.
- An unknown field (for example `message`) or an unknown `cause` gives 400 `invalid_health` and
  no log line.
- A read credential gives 403.

Screenshot harness: one new state capture of `unresponsive`, if the existing shots config makes
that a one-line addition.

## README

- A paragraph beside "Startup timing": **When the voice service stops answering**. It covers
  the triggers, the threshold, the UI state, the log line, and the "cannot observe" list above.
- One row in the endpoint table for `POST /api/v1/voice-health`.
- One sentence in the `vibe-talk-v1` protocol section: a turn a server completes with neither
  audible audio nor text is reported to the listener as the service not responding, so a server
  that cannot speak should send `error`, not silent PCM.

## Changes after review

- **One status text** for every cause. A second wording for typed `no_reply` told the reader
  nothing they could act on differently.
- **A server-side line budget.** The page reports each cause once per call and a recovery once per
  reported episode, at most seven lines a call. The server cannot enforce "per call" because the
  record deliberately carries no call id, so it enforces a per-process budget instead: 30 lines a
  minute, then 429. The body cap is 512 bytes.
- **Every field is required**, so a partial record is refused like an unknown one.
- **The empty answer to closing the page's own audio segment is neutral.** Switching from the
  microphone to typing sends `audio_end` and waits for a `turn_complete` that may carry nothing;
  that turn asked for no reply, so it is not counted as silent.
- **Threshold constants** (`AUDIBLE_PEAK`, `SILENT_TURNS_TO_REPORT`, `NO_REPLY_MS`) sit together
  in `web/voice.js`, each with its reason, and each has a tuning band in the page tests.
- A single silent turn mid-conversation never reports: a turn that only calls a tool may be silent.
- **An `error` frame is its turn's verdict.** A server may still close the failed turn with an
  ordinary `turn_complete`, and the `error` may arrive before or after that turn's audio. That
  turn is not counted as silent, and the no-reply bound is disarmed, so one failure logs one cause
  and the red error state is not replaced by `unresponsive`.
- **A missing transcript frame counts the same as an empty one.** A second observed failure had a
  greeting of all-zero PCM with no transcript frame at all; its greeting turn ended about 1.9 s
  after it began, so the silent-greeting rule, not the 15 s bound, catches it. A turn with no audio
  bytes and no transcript frame is tested too.
- **The `turn_complete` that acknowledges the page's own `audio_end` is a boundary, not a reply.**
  A server may send it immediately. It is neutral (see above), and a test harness driving this protocol
  must consume it before sending the next input rather than take it for the prompt's answer.
- **An interrupted turn that cut short half a second or more of inaudible audio counts as silent**
  (`INTERRUPTED_SILENT_MS` = 500). A listener hearing nothing says "hello?" over it; if the server
  takes that for barge-in, every dead turn would otherwise be marked interrupted and hide the
  failure. Inaudible means no sample in the whole turn reached `AUDIBLE_PEAK`, not a sum of quiet
  samples: healthy speech has well over a second of sub-threshold samples between its words (in
  one healthy reply only about 39% of samples reached it), and a test pins that shape as heard.
  Dead turns have been seen as short as 0.9 s, which is why the bound is under a second.
- **Several causes may be logged in one episode**, each once per call: for example `no_reply` for
  a greeting that is slow to finish, then `silent_greeting` when it completes silently.
- **An `error` carries no turn number**, so its verdict attaches to the next `turn_complete`. An
  `error` sent between turns therefore spares the next turn instead of the one before it.
- **Throttling is visible**: the first refused record in each window writes one
  `voice_health_throttled` line.
- **A typed call's greeting** is judged as an ordinary turn and is not timed, because a typed call
  does not wait for its greeting. `vibe-talk-v1` does not say whether a server greets a client that
  never sends `audio_start`, or which turn number a greeting carries. Waiting for the greeting would
  hold every typed call's first prompt behind a greeting that may never come.
  - **What is handled:** once any assistant audio or text has arrived since the last
    `turn_complete`, a turn is plainly under way. A prompt typed then is held until that turn
    completes. While it waits, the 15 s bound restarts with each further piece of the turn, so it
    measures the turn going quiet without completing. (A typed call plays no PCM and never judges
    it heard, so a bound that ran only once would report a greeting that simply outspoke it.)
  - **Known limitation:** a prompt sent BEFORE any of the greeting has arrived is not held. That
    includes one queued before `session_started`. The page cannot tell the greeting from the reply:
    - the greeting's first text disarms the prompt's 15 s bound (PCM does not, in a typed call);
    - the greeting's `turn_complete` is taken as the prompt's;
    - a second queued prompt is sent while the first is still being answered.

    A page test pins this, so closing it is a deliberate change. Closing it needs the protocol to
    define both points.
- **A heard turn always ends the silent run**, even one that is not judged because it was
  interrupted or carried an `error`. Otherwise a silent turn before it and one after it would be
  reported as two in a row.
- Healthy reference, provider-neutral: first assistant audio about 0.86 s after the greeting was
  requested, and about 0.06 s after a typed prompt.

## Out of scope

- Any change to a deployment's bridge or its configuration.
- Retrying or reconnecting automatically. That is a later decision, once the signal shows how
  often this happens.
