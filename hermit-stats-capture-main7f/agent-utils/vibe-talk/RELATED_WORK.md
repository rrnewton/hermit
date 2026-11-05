# Voice agent bridge related work

Working name for the proposed project: `vibe-talk`. Research checked 2026-08-18 against project
source code, vendor documentation, and package metadata. Where a claim rests on reading source, the
file is cited; where it rests on vendor documentation, the doc page is linked; claims that could not
be confirmed are marked **unverified** and are never used to carry a conclusion.

## Short labels for repository links (2026-10-08)

`#227 github-link-abbrev` draws a link whose text is its own GitHub address with a short label,
`widgets#123` rather than the address. The convention is GitHub's own:
[autolinked references](https://docs.github.com/en/get-started/writing-on-github/working-with-advanced-formatting/autolinked-references-and-urls)
shortens an issue or pull request address to `OWNER/REPO#N` (or `#N` within the same repository),
a commit to `OWNER/REPO@SHA` with the hash cut short, and a comment's address to the same with
`(comment)` after it. The labels here follow those forms and differ in one respect, at the owner's
request: the repository's owner is left out, because in a status line that names a dozen pull
requests the owner is the same dozen times. A label's address stays the link's address, and the
full address is the link's hover text. GitHub's forms cover only issues, pull requests and commits;
the labels for comparisons, releases, runs and files here have no upstream convention to follow,
and an address of any other shape is left as written.

GitHub leaves an address inside code as written, and so does this server, for the reason that it
relabels only what the parser made a link: the labels are written onto comrak's parsed tree
(`src/render.rs`), never by a pattern over the source, and code holds no links.

## Rendering message Markdown on the server (2026-10-08)

`#217 markdown-blocks` replaced the page's own hundred-line Markdown renderer with rendering on
the server, the arrangement public forges with their own servers use: the server owns one
renderer and one sanitizer, and the browser inserts sanitized HTML.
[crates.io](https://github.com/rust-lang/crates.io/tree/main/crates/crates_io_markdown) renders
README files with comrak and sanitizes them with ammonia; GitHub renders with its fork of cmark,
[cmark-gfm](https://github.com/github/cmark-gfm), and sanitizes the result before it reaches the
page. Chat messages need the same GitHub-flavoured extensions those READMEs do — above all bare
addresses as links, which chat is full of.

Three maintained Rust CommonMark parsers were compared:

* [comrak](https://github.com/kivikakk/comrak) is a port of cmark-gfm. It implements every GFM
  extension, including the extended autolinks, has a hard-line-break option for chat's
  one-newline-one-break rule and an option to escape rather than pass raw HTML, and exposes the
  parsed tree for adjustment before it is written. That last property is what lets the provider
  dialects here — mention chips, one-asterisk bold for Google Chat and Slack, images as links — be
  small edits to a parsed tree instead of a second parser. Chosen; 0.55, because 0.56 raised its
  minimum Rust above this crate's.
* [pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark) is a fast pull parser with GFM
  tables, strikethrough and task lists, but it does not implement GFM's extended autolinks, so a
  bare `https://…` stays text — the most common link form in chat.
* [markdown-rs](https://github.com/wooorm/markdown-rs) implements GFM including autolink literals and
  can produce a syntax tree, but its HTML is written from its own events: there is no way to adjust
  the tree and then write that tree as HTML.

For sanitizing, [ammonia](https://github.com/rust-ammonia/ammonia) is an allowlist sanitizer built
on html5ever, the HTML5 parser from Servo, so it parses its input the way a browser would before
deciding what survives. The client-side alternative,
[DOMPurify](https://github.com/cure53/DOMPurify), would have meant shipping a renderer and a
sanitizer to every phone and keeping the page dependency-free no longer.

## Device speech for individual messages (2026-09-19)

Reading a selected message does not require a conversational agent. Chromium's implementation of
the Web Speech API calls Android's installed `TextToSpeech` service, so a browser can offer
tap-to-read without shipping a speech model, obtaining an API key, or routing synthesis through
the application server. This is a smaller dependency than a hosted voice conversation when the
desired interaction is choosing and listening to an existing message.

[MDN's voice discovery reference](https://developer.mozilla.org/en-US/docs/Web/API/SpeechSynthesis/getVoices)
describes `getVoices()` and the asynchronous `voiceschanged` event. Its
[compatibility data](https://github.com/mdn/browser-compat-data/blob/main/api/SpeechSynthesis.json)
lists Chrome Android support, but no Android WebView support. Chromium's
[Android implementation](https://chromium.googlesource.com/chromium/src/+/main/content/browser/speech/tts_android.cc)
also explains why a web client should use cancellation and fresh utterances: Android pause stops
speech, while resume has no implementation.

The [localService property](https://developer.mozilla.org/en-US/docs/Web/API/SpeechSynthesisVoice/localService)
allows a client to select browser-reported local voices. That is not an independent audit of the
phone's speech engine: Chromium's
[Android service integration](https://chromium.googlesource.com/chromium/src/+/main/content/public/android/java/src/org/chromium/content/browser/TtsPlatformImpl.java)
uses the configured engine and exposes language entries rather than checking every native voice's
network requirement. Downloaded voices and an offline device check are appropriate when offline
processing is required. Background playback and physical-phone audio remain unverified by desktop
browser automation.

The device backend covers selected-message reading. The existing ElevenLabs integration still
supplies its own server-generated audio and conversational voice features; device speech does not
provide speech recognition, model responses, or summarisation.

## The problem being solved

The proposal is a Rust server, run as a Podman container, that receives Discord webhook pushes,
serves a web app to a phone, and brokers an ElevenLabs voice agent per monitored channel so the
owner can talk to his coding agents from the car.

That is the proposed *solution*. The stated *problem* is narrower and different:

> "You make extremely verbose messages in discord and sometimes you'll send 10 or more of them by
> the time I come back and check the channel. So even if I had good mobile support to read my
> screen, it wouldn't be enough."

The load-bearing capability is therefore **triage of a channel backlog** — turning ten verbose agent
messages into one sentence a driver can absorb — with voice as the delivery mechanism and a reply
path back to the channel. A candidate with excellent speech and no triage is *worse* than the status
quo, because it reads all ten messages aloud instead of letting you skim past nine of them. Every
candidate below is scored on triage first and speech second.

## Direct answer

**Do not build the server yet.** Two compositions of existing products cover the stated problem, and
neither requires writing a voice server. They should be tried in cost order:

1. **Claude mobile voice mode plus a remote Discord MCP connector.** Voice mode is already in the
   apps he pays for, and Anthropic documents that connected tools work inside voice mode, including
   for exactly this shape of request — "catch you up on your email... or summarize a thread". The
   only new infrastructure is one hosted Discord MCP server. **This hinges on one unverified fact**
   (below) and is a half-day experiment.
2. **An ElevenLabs agent with a phone number plus the same Discord MCP server.** No app, no web
   client, no client code at all: he calls a phone number from the car and talks to an agent that can
   read and post to Discord. This is the strongest fit for the car specifically, and it reuses the
   Discord MCP server from option 1.

**Happy Coder is the best-engineered thing in this space and the right reference implementation, but
it is not the answer**, because it is wired to Claude Code and Codex sessions rather than to Discord.
Its value here is that its source settles several design questions empirically — including the
surprising one that it has no summarization layer at all.

If both options fail, the project that remains is not the proposed one. The Rust web server, the
webhook receiver, the session multiplexer, and the tap-to-talk UI all fall away; what is left is a
**Discord MCP server plus a hosted agent configuration**, which is a fraction of the scope.

## At a glance

`Partial` marks a capability that exists but with materially different semantics, or that requires
an adapter.

| Candidate | Triages a channel backlog | Voice in/out | Posts back to Discord | Works hands-free in a car | New code required |
| --- | --- | --- | --- | --- | --- |
| Proposed `vibe-talk` server | Yes (to be built) | Yes | Yes | Partial (phone screen) | Whole server |
| Claude mobile voice mode + Discord MCP | Yes | Yes | Yes | Partial (phone screen) | MCP server only |
| [ElevenLabs Agents](https://elevenlabs.io/docs/eleven-agents/customization/tools/mcp) + phone number + Discord MCP | Yes | Yes | Yes | **Yes** (a phone call) | MCP server only |
| [Happy Coder](https://github.com/slopus/happy) | Partial (terseness, not summarization) | Yes | No (agent sessions, not Discord) | Partial | Fork + Discord backend |
| [Omnara](https://omnara.com/) | Partial | Yes | No | Partial | Not extensible |
| Discord TTS bots (SeaVoice, Text To Speech Bot) | **No** | Yes | Partial | Partial | None |
| Discord summarizer bots ([Discord-AI-Summarizer](https://github.com/ThatSINEWAVE/Discord-AI-Summarizer)) | Partial (on demand, text) | No | Partial | No | None |
| Voice frameworks ([Pipecat](https://github.com/pipecat-ai/pipecat), [LiveKit Agents](https://github.com/livekit/agents)) | No | Yes | No | No | Whole server |

The row that matters most is the Discord TTS bots one. They are the obvious "cheap answer" and they
score **No** on the only column that counts: reading messages aloud in order is a faithful rendering
of the backlog, which is precisely the thing that does not work.

## Happy Coder

[Happy](https://github.com/slopus/happy) (`slopus/happy`, MIT) is a mobile, web, and desktop client
for Claude Code and Codex with an embedded ElevenLabs voice agent. It is actively maintained — at
review time `origin/main` was at `eb980a5c`, dated 2026-08-10. This review read a local checkout of
that revision, and the owner maintains a fork at `rrnewton/happy-devbox`.

Its voice design is documented in-repo at
[`docs/voice-architecture.md`](https://github.com/slopus/happy/blob/main/docs/voice-architecture.md)
and [`docs/paid-voice.md`](https://github.com/slopus/happy/blob/main/docs/paid-voice.md), and the
implementation is about 500 lines under `packages/happy-app/sources/realtime/`. The important
findings come from reading that source rather than from the marketing.

### What Happy confirms about the design

- **It uses the hosted ElevenLabs Agents platform, not a self-assembled stack.** The dependencies are
  `@elevenlabs/react` and `@elevenlabs/react-native`; speech-to-text, the LLM, turn-taking, and
  text-to-speech all run inside one ElevenLabs session over WebRTC. The client is a thin bridge.
- **The voice agent acts through client tools, not through the transport.** `realtimeClientTools.ts`
  exposes exactly two: `sendMessageToSession` and `processPermissionRequest`. This is the pattern the
  proposal wants — the agent talks, and a small tool posts on the user's behalf.
- **Multiplexing across sessions needs no UI switcher.** A single module-level `currentSessionId`
  routes tool calls, and the agent is handed a "session directory" listing every active session at
  start. The user says which one they mean. This is directly relevant: the proposed per-channel
  tap-to-talk UI may be solving by widgets what the agent can solve by listening.
- **Two distinct context channels, and this is the subtle part.** `sendContextualUpdate()` injects
  information *silently* — the agent learns it but does not speak. `sendTextMessage()` acts as a user
  turn and forces a reply. Incoming agent chatter uses the silent channel; only completions and
  permission requests force speech.
- **Backlog batching already exists.** While anyone is speaking, prompts queue in `pendingPrompts[]`
  and are flushed as a *single joined turn* when the mode returns to idle. This is Happy's answer to
  the ten-messages-arrived problem, and it is a good one.

### What Happy confirms about the *triage* problem, which is the decisive finding

**Happy has no summarization pass.** There is no reducer, no rollup, no "summarize the backlog" step
anywhere in the voice path. `contextFormatters.ts` formats messages nearly verbatim into the agent's
context, and `voiceConfig.ts` caps history with `MAX_HISTORY_MESSAGES: 50` — truncation, not
summarization.

Triage is instead achieved entirely by *prompting the voice model to be terse*. From
`voiceSystemPrompt.ts`:

> "You always answer using a single sentence. When you are talking to a person be very short until
> explicitly asked to elaborate."
>
> "Human understands stuff better than you, do not explain if not asked."

and, for the completion case, from `contextFormatters.ts`:

> "Claude Code done working in session: … The previous message(s) are the summary of the work done.
> Report this to the human immediately."

This matters a great deal for the proposal. A shipped, funded product that hit exactly this problem
solved it with a system prompt and a 50-message cap, not with a summarization pipeline. That is
strong evidence that **the triage capability is a prompt, not a subsystem** — which is why the
no-code compositions below are credible, and why a bespoke Rust summarizer would be building the
easy part.

The one real cost of the prompt-only approach is that context is bounded by truncation: a backlog
longer than 50 messages silently loses its oldest entries rather than being compressed. For a channel
the owner checks a few times a day this is probably fine; if it is not, a summarization step is the
one component genuinely worth writing, and it is small.

### Why Happy is nonetheless not the answer

Happy's integration point is wrong. It attaches to agent sessions through its own wrapper — you run
`happy claude` or `happy codex` instead of `claude` or `codex` — and the phone talks to *those*
sessions. It has no notion of a Discord channel, and pointing it at one would mean writing a Discord
session backend inside a React Native monorepo, which is more work than either option in the direct
answer.

There is a real alternative hiding here that should be stated plainly, because it dissolves the
problem rather than solving it: **if the fleet ran under Happy, Discord would not be the mobile
interface at all.** Happy also ships `happy-agent`, a "remote agent control CLI (create, send,
monitor sessions)", which is a programmatic way to inject and monitor sessions. Whether the existing
multi-host tmux/Codex/Antigravity fleet can be wrapped this way is not something this review
established, and it would be a significant change to the harness. It is listed as an option to
consider, not a recommendation.

Two further practical notes from `docs/paid-voice.md`: Happy's hosted voice is metered (20 free
minutes, then a paywall, hard-blocked at 5 hours per 30 days), but it supports a **bring-your-own
agent ID bypass** that is explicitly "Unlimited, $0, user's own ElevenLabs" and skips the gating
entirely. Their measured cost for ElevenLabs voice was **about $0.01 per minute** ($1600 across 171K
minutes) — a useful, independently measured budget figure for any option here.

## Claude mobile voice mode plus a Discord MCP connector

This is the cheapest candidate and it was nearly missed, because the first search result claimed the
opposite of the truth. An initial search asserted that "MCPs are not available as tools while voice
mode conversation is running". Anthropic's own help centre contradicts this:

> "In voice mode, Claude can use the tools you've connected, like Gmail, Google Calendar, Google
> Docs, and Slack. You can ask Claude to catch you up on your email, check your calendar before a
> meeting, or summarize a thread without leaving the conversation."
> — [Use voice mode](https://support.claude.com/en/articles/11101966-use-voice-mode)

"Catch you up" and "summarize a thread" is the requirement, stated by the vendor, as a supported use.
Voice mode is beta and available on all plans across Claude Mobile (iOS and Android), Desktop, and
web. Custom connectors built on
[remote MCP](https://support.claude.com/en/articles/11503834-build-custom-connectors-via-remote-mcp-servers)
work across Claude.ai, Desktop, and the mobile apps; mobile cannot run local MCP servers, so the
Discord server must be remote and HTTPS-reachable.

**The unverified fact this rests on:** Anthropic's voice-mode documentation names only first-party
integrations (Gmail, Calendar, Docs, Slack). It does **not** state whether a *custom remote MCP
connector* is callable inside voice mode. This review could not confirm it either way, and it is the
single pivot of the cheapest option. It is also trivially testable: stand up any remote MCP server,
connect it, open voice mode, and ask the agent to call one of its tools. **That test should be the
first thing done on this project**, ahead of any design work.

Known limits even if it works: voice mode is a chat, not a monitor — there is no push, so he must
initiate ("catch me up on the acme channel"). For a driver that is arguably correct behaviour.
Messages would be posted by the bot identity the MCP server authenticates as, not by his own account.

## ElevenLabs Agents plus a phone number

This is the strongest fit for the car, and it is the composition worth building toward if option 1
fails. It is also, notably, the same platform Happy chose, so Happy's source doubles as a worked
example of the hard parts.

- **Remote MCP is supported.** ElevenLabs Agents connect to remote MCP servers over
  [SSE or streamable HTTP](https://elevenlabs.io/docs/eleven-agents/customization/tools/mcp), with a
  secret token or custom headers for auth, and three approval modes: always-ask, per-tool
  fine-grained approval, or no approval. Per-tool approval maps well onto "reading is automatic,
  posting asks first".
- **Phone numbers are native.** Agents can be attached to a
  [Twilio number](https://elevenlabs.io/docs/eleven-agents/phone-numbers/twilio-integration/native-integration)
  or to an existing number by
  [SIP trunking](https://elevenlabs.io/docs/eleven-agents/phone-numbers/sip-trunking), without
  porting. This deletes the entire client half of the proposal: no web app, no phone UI, no
  tap-to-talk button, no push-notification story. In a car it is strictly better than any app,
  because a phone call is the one thing every car audio system already handles, and it needs no
  screen.
- **A hosted web widget exists too**, embeddable in one line of HTML with a shareable link, if a
  screen-based path is wanted alongside the phone number.
- **Turn-taking and barge-in are built in**, driven by a proprietary endpointing model.

Costs and caveats, stated honestly. ElevenLabs does not expose tuning for its turn-taking or custom
interruption logic; conditional interruption must be built client-side, which the telephony path
cannot do. MCP is unavailable to accounts on Zero Retention Mode or requiring HIPAA. Running the
agent hosted means channel content and summaries transit ElevenLabs, and the Discord bot token lives
in the MCP server, which must be publicly reachable and therefore properly authenticated — that
server is the real security boundary of this design and deserves more care than the rest of it
combined. Budget roughly $0.01/minute of conversation plus telephony.

## Discord-side options

These were evaluated and rejected, but the reason is worth recording because it is the whole thesis
of the project.

**Text-to-speech bots** — [SeaVoice](https://voice.seasalt.ai/discord/), the Discord App Directory's
Text To Speech Bot, [DiscordSpeechBot](https://github.com/inevolin/DiscordSpeechBot),
[Discord-Voice-Channel-Bot](https://github.com/Gemeri/Discord-Voice-Channel-Bot) — read messages
aloud in a voice channel. They are the cheapest possible thing and they fail the actual requirement
exactly. Reading ten verbose messages aloud in order is *worse* than not reading them, because
speech cannot be skimmed: the reader can skip nine messages in two seconds and the listener cannot.
Fidelity to the backlog is the bug, not the feature.

**Summarizer bots** — [Discord-AI-Summarizer](https://github.com/ThatSINEWAVE/Discord-AI-Summarizer)
and similar — do the right operation but deliver it as text back into the channel, on demand, which
returns the owner to reading his phone. They are useful as evidence that the summarization half is
routine, and one could in principle be paired with a TTS bot, but the seam between two hobbyist bots
is not obviously less work than option 2 and offers no conversation, no follow-up questions, and no
reply path.

**Discord's own voice features** are for human-to-human voice channels; a bot in a voice channel is
the TTS path above. Whether Discord's own AI channel-summary feature is available and adequate on his
server was **not investigated** and is a cheap thing to check before anything else is built.

## The interruption and pause requirement

He called this out specifically, and the research suggests it is already solved — by a different
mechanism than the one he asked for.

- **Barge-in is native and automatic** on ElevenLabs: speaking over the agent cancels the in-flight
  response, and the TypeScript SDK exposes an `AbortSignal` that fires on interruption so the
  upstream LLM call is cancelled too. He does not need a pause button to stop an explanation; he
  needs only to talk.
- **`skip_turn` is a native system tool** that lets the agent
  [explicitly hold and wait](https://elevenlabs.io/docs/agents-platform/customization/tools/system-tools/skip-turn)
  for the user to re-engage. Happy already leans on it in its system prompt: "You MUST call skip_turn
  tool if you believe the speaker is talking to some other human in the room." Spoken "hold on" and
  "go ahead" therefore become pause and resume, with no UI at all.
- **Happy's own mode model** (`idle` / `user-speaking` / `agent-speaking`, with agent winning ties
  because simultaneous speech is probably crosstalk) is a good reference if a client is ever built.

So the pause/resume requirement is met conversationally on every candidate. What is genuinely *not*
available on the telephony path is **hold-to-talk**, which needs a screen and a finger. That is worth
weighing against the fact that hold-to-talk is also the requirement least compatible with driving.

> **Correction, 2026-08-19, from running it.** The paragraph above was written from vendor
> documentation and it overstates its case. Barge-in and `skip_turn` are real, but neither is
> pause: barge-in interrupts the agent *while it speaks*, `skip_turn` is the agent electing to
> hold, and in both the socket stays open and the microphone keeps streaming. There is no vendor
> pause primitive; the only transport-level action is hanging up. What actually pauses is this
> project's own client-side mute, which stops the upload and keeps the context.
>
> Two consequences the documentation does not mention and the owner hit in use: **billing
> continues while muted**, since a conversation is billed for being open — the vendor discounts
> silent periods but does not stop the meter — and **the agent starts asking whether you are still
> there**, because a client-side mute is invisible to it. From the vendor's side, "muted" and
> "went quiet" are indistinguishable, so our own pause is what provokes the prompting.

## What is genuinely unmet in open source

Stating this precisely, since it is the fallback justification for building:

No existing project monitors a Discord channel, triages an accumulated backlog of verbose agent
messages into speech-sized summaries, converses about them, and posts replies back to the channel as
one integrated thing. That combination does not exist off the shelf.

But it decomposes cleanly into two pieces that both do exist — a Discord MCP server (several
implementations) and a hosted conversational agent that can call MCP tools and be reached by phone
(ElevenLabs, and possibly Claude's own voice mode). The unmet need is the *composition*, and a
composition of two supported products is configuration, not a Rust server.

## Recommendations on the specification

Offered because the research contradicts parts of it, not to relitigate settled choices.

1. **Test custom-remote-MCP-inside-Claude-voice-mode first.** It is half a day and it can end the
   project. Nothing else should start before it.
2. **Build the Discord MCP server first regardless.** It is the one component every path needs,
   including the build path, and it is the only piece with real security weight.
3. **Reconsider the web app.** A phone number reaches the car better than any web app, needs no
   screen, no push notifications, and no client code. If a screen path is still wanted, the
   ElevenLabs hosted widget is one line of HTML.
4. **Reconsider the per-channel tap-to-talk UI.** Happy multiplexes sessions with a spoken directory
   and a routing variable, not a switcher. Channels are the same shape of problem.
5. **Drop the pause button.** Barge-in plus `skip_turn` cover it natively and hands-free; a button
   is the worse interface for the stated use case.
6. **Keep webhook push only if push is genuinely wanted.** Options 1 and 2 are both pull ("catch me
   up"), which suits driving. Webhook ingestion only earns its complexity if he wants the agent to
   call *him*, which he did not ask for.
7. **If it is built, build the summarizer, not the plumbing.** The truncation-not-summarization gap
   in Happy's design is the single place where a bespoke component would add something the
   off-the-shelf compositions lack.

## Making markdown pronounceable

Added 2026-08-24, when read-aloud was found saying "asterisk asterisk deploy asterisk asterisk",
spelling out nineteen-digit snowflakes, and reading ISO timestamps character by character.

**Sourcing caveat, stated first.** This host's egress proxy refuses the pages below to this agent's
identity, so none of them was fetched. What follows is a survey from established knowledge with
pointers for a reader who *can* open them; no wording is quoted, and no claim here should be
treated as verified. Search surfaced the URLs; nothing read them.

### Screen readers are the closest prior art, and they solved the table problem first

NVDA, JAWS and VoiceOver have spent two decades on exactly this question: how to speak a document
whose structure is visual. Their answer for tabular data is not to read the table — it is to
**announce its shape and let the listener decide**, in the form "table with N columns and M rows",
after which the user may step through it or skip it entirely.

That is the idiom `speakable::describe_tables` implements, and arriving at it independently is
weak evidence it is right; the accessibility literature is much stronger evidence. The one thing
this bridge does that a screen reader cannot is **decide for the listener**, because there is no
way to step into a table through a phone speaker in a car. So it also offers the first column's
keys when those read as words — the part that says what the table is *about*.

Worth reading against our choices:

- NVDA user guide, table navigation and reporting: <https://download.nvaccess.org/releases/2024.4beta2/documentation/userGuide.html>
- PowerMapper's cross-reader table comparison: <https://www.powermapper.com/tests/screen-readers/tables/>
- How screen readers get from DOM to speech: <https://testparty.ai/blog/how-screen-readers-parse-dom>

### Text normalization is a named field, and this is a small instance of it

Turning written text into speakable text is **text normalization** (TN) — expanding numbers,
dates, currency, abbreviations and symbols into the words a speaker would actually say. It is the
front half of every production TTS pipeline, classically as hand-written grammars (Google's
Kestrel, built on finite-state transducers) and latterly with neural models, which introduced the
failure this module is careful to avoid: a learned normalizer will occasionally produce a
*confidently wrong* reading of a number, where a rule produces a clumsy one.

Sproat and Jaitly's "RNN Approaches to Text Normalization: A Challenge" is the standard reference
for why that trade matters. Our transformations are all deterministic and none of them calls
anything, which was the owner's explicit requirement — no extra tokens — and is also the safer
half of that trade.

- Azure's *display text formatting* is the same machinery pointed the other way (inverse text
  normalization, speech → written form): <https://learn.microsoft.com/en-us/azure/ai-services/speech-service/display-text-format>

### SSML is the alternative we did not take

The standards answer to "say this differently" is **SSML**: wrap the text in markup and let the
engine decide, with `<say-as interpret-as="date">`, `<sub alias="…">` for substitutions, and
`<break>` for pacing. It is more expressive than rewriting the string and it keeps the original
text intact.

We rewrite anyway, for three reasons worth recording:

1. **Support is uneven.** SSML coverage differs per vendor and per voice, and a tag a vendor
   ignores fails silently — the listener hears the raw thing, which is the bug being fixed.
2. **Half our transformations have no tag.** There is no `<say-as>` for "this forty-character hex
   string is the same one you heard a sentence ago". Placeholder naming is a semantic decision, not
   a pronunciation hint.
3. **It would couple us to one vendor's dialect** at exactly the layer we keep swappable
   (`SpeechProvider`).

If a future backend has strong SSML, dates and numbers could move to `<say-as>` while placeholders
stay here. That would be a genuine improvement and it is not a rewrite.

### Read-it-later products face the same problem and mostly punt

Speechify, Pocket's read-aloud, and ElevenLabs' own Reader all take arbitrary web or markdown
content to speech. Publicly they say little about preprocessing, and the observable behaviour of
several is that code blocks and tables are read verbatim or dropped wholesale with no announcement
— which is the failure mode on either side of what this module tries to do: reading noise, or
silently removing content the listener was told to look at. Our thresholds exist to sit between
them: a snippet of three lines or fewer is spoken, because at that length it *is* the message; a
longer one is named with its language and size, because the listener should know what they are
choosing not to hear.

### What is unmet

No open-source library found does markdown-to-speakable with **stable within-document placeholders**
— the property that makes "hash code A" useful, because it lets a listener tell "the same commit"
from "a different commit" without hearing either. Ordinary markdown strippers (`remark-strip-markdown`,
`pandoc -t plain`) remove syntax and leave the hash. That gap is why this is code here rather than
a dependency.

## Sources

- [slopus/happy](https://github.com/slopus/happy) — voice architecture, paid-voice, and
  `packages/happy-app/sources/realtime/` read at `origin/main` `eb980a5c` (2026-08-10)
- [Happy: Why Voice Coding Makes Sense](https://happy.engineering/docs/features/voice-coding-with-claude-code/)
- [ElevenLabs Agents: MCP](https://elevenlabs.io/docs/eleven-agents/customization/tools/mcp)
- [ElevenLabs Agents: Twilio native integration](https://elevenlabs.io/docs/eleven-agents/phone-numbers/twilio-integration/native-integration)
- [ElevenLabs Agents: SIP trunking](https://elevenlabs.io/docs/eleven-agents/phone-numbers/sip-trunking)
- [ElevenLabs Agents: skip turn](https://elevenlabs.io/docs/agents-platform/customization/tools/system-tools/skip-turn)
- [ElevenLabs Agents: conversation flow](https://elevenlabs.io/docs/eleven-agents/customization/conversation-flow)
- [Claude: Use voice mode](https://support.claude.com/en/articles/11101966-use-voice-mode)
- [Claude: Build custom connectors via remote MCP servers](https://support.claude.com/en/articles/11503834-build-custom-connectors-via-remote-mcp-servers)
- [Discord MCP servers guide](https://mcp.directory/blog/discord-mcp-complete-guide-2026),
  [barryyip0625/mcp-discord](https://github.com/barryyip0625/mcp-discord),
  [v-3/discordmcp](https://github.com/v-3/discordmcp)
- [Omnara](https://omnara.com/), [Pipecat](https://github.com/pipecat-ai/pipecat),
  [LiveKit Agents](https://github.com/livekit/agents)
- [ThatSINEWAVE/Discord-AI-Summarizer](https://github.com/ThatSINEWAVE/Discord-AI-Summarizer),
  [SeaVoice](https://voice.seasalt.ai/discord/),
  [inevolin/DiscordSpeechBot](https://github.com/inevolin/DiscordSpeechBot)
