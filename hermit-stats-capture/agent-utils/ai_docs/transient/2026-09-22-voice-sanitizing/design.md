# Pre-sanitizing what the voice agent is given to say

Working design, 2026-09-22. Transient: fold what survives into the code and
`common/docs/`, and delete this.

## The complaint

> I want optional pre-sanitizing of messages on their way from discord to the
> voice agent. I thought we already had something for this. But what I
> specifically want is normalization of the time zones where we match against
> UTC times and convert them to simple undecorated local times and also reduce
> their precision taking off the seconds field because that is not relevant and
> not worth wasting voice speech on. I also want us to substitute all long
> numeric identifiers and possibly also hash codes if we can recognize them.
> […] I think we should maintain per session or per transcript substitution
> list. When a long number is mentioned the first time maybe it is assigned
> letter A. When the next long number is mentioned, maybe it is assigned letter
> B.

## "I thought we already had something for this" — correct

`src/speakable.rs` (750 lines) already does three of the four jobs:

| Job | Where | State |
|---|---|---|
| Markdown off | `strip_markdown` | done |
| Times spoken | `speak_times` → `say_instant` | done, but **relative** |
| Long digits → letters | `name_opaque_strings` → `name_for` → `letter_for` | done, but **per call** |
| Hashes → letters | same, `HASH_MIN_CHARS = 12` | done, but **per call** |

`BIG_NUMBER_MIN_DIGITS = 10` already covers a snowflake and a Unix epoch while
deliberately sparing years, ports and counts. The letter assignment already
reuses a letter for a repeated value inside one message, which is the property
that makes "the same one" distinguishable from "a different one".

So the request is not "build this". It is three specific gaps.

## Gap 1 — it is wired to exactly one route

`speakable::for_speech` has one production caller: the read-aloud route in
`src/http/api.rs`, which generates audio server-side. Every other path that puts
channel text in front of the voice agent bypasses it.

**`digest_channel`**, the tool the agent calls for a summary, emits

    [message id | local time | exact instant | author <@author id>] summary

so the model is handed a nineteen-digit message id, a nineteen-digit author id,
and an ISO instant, and its own tool description instructs it to "say it exactly
as written". This is the reported symptom — long ids read aloud — arriving
through the tool surface rather than through the text.

**The read-new relay** added by `#126 read-new-selector` builds its quoted lines
in the browser, from `message.content`, with no sanitizing at all. The server
never sees the turn. So the newest path to the vendor is the least sanitized
one, and a message containing a snowflake will be read out digit by digit in
`full` mode.

Nothing carries a sanitized body to the page: `Message` has `content` (raw) and
`spoken_time`, and no `spoken_content`.

## Gap 2 — times are relative, and the digest keeps seconds

`say_instant` produces "three hours ago", which is the right call inside a
sentence. But `clock::spoken` — what the digest line and `Message::spoken_time`
carry — is `%H:%M:%S %Z`, giving `09:51:25 EDT`. Both the seconds and the zone
label are what the complaint names. Wanted: `09:51`, undecorated.

The zone is already resolved correctly and in two places for two good reasons
(the server's configured zone for the agent, which cannot ask a browser; the
browser's own zone for the phone, per `#52 operator-timezone`). That split
stays; only the format changes.

## Gap 3 — the substitution table is per message

`name_for` takes `seen: &mut BTreeMap<String, String>` built fresh inside each
`for_speech` call. So author 1234… is "A" in one message and "A" again in the
next only by coincidence of ordering — and across the digest/relay/read-aloud
paths there is no shared table at all.

Wanted: one table per session or per transcript, so "user id A" names the same
account for as long as the reader is listening.

## Shape of the change

1. **Lift the table out of the call.** `for_speech` grows a caller-supplied
   table; the existing signature keeps working by passing a fresh one. The table
   lives beside the session, not in a global — two readers must not share
   letters, and a letter must not outlive the transcript that explains it.
2. **Sanitize the digest line**, and drop seconds and the zone label from the
   spoken time. The "exact instant" field stays: it is explicitly for computing
   with, not for reading.
3. **Sanitize the relay turn.** Either the page sends ids to a server endpoint
   that returns the sanitized turn, or the server ships a sanitized body
   alongside `content`. The second is fewer round trips on the path that is
   already latency-sensitive, and keeps the browser from holding the table.
4. **Optional, as asked** — a setting, on by default, with the cost stated where
   the reader can see it.

## Open question

Whether the reader ever needs to hear a real id. A table that is only ever
spoken as letters is unambiguous out loud and useless if the reader then wants
to paste one. Leaning: the letters are for speech, and the on-screen row keeps
showing the real value, so the mapping is always recoverable by looking.
