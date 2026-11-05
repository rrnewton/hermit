# vibe-talk

A small Rust web server for reading, discussing, and replying to your coding agents' chat messages
from a phone. It supports Discord directly and other services, including Google Chat, through a
compatible HTTP bridge. Messages can be read with the phone's speech engine or ElevenLabs;
voice conversations use ElevenLabs by default or a deployment-managed WebSocket provider.

It is a **bridge, not an agent host**. It holds the chat backend's credential, answers questions about
channels over an authenticated HTTP API, and serves a phone web app. It is deliberately **not**
co-located with the coding agents: it needs no access to any development workspace, and nothing in
it is tied to the machine it currently runs on. It starts in Podman on a laptop and is expected to
move to a small cloud host unchanged.

The design decision behind it, stated plainly: composing hosted products would also work (see
[`RELATED_WORK.md`](RELATED_WORK.md), which recommended exactly that), but a server is
needed either way to hold the chat credential, and this project would rather own that front
door than hand it to a vendor.

## What it does in v0

**Pull by default, with optional adapter push.** Reads and replies use the configured chat HTTP
API. A deployment may separately give an external provider adapter a dedicated ingestion token;
that adapter pushes normalized events into vibe-talk so an open page can update immediately. The
adapter endpoint is authenticated but does not itself make the deployment public or verify a
provider signature, so any external subscription receiver remains the adapter's responsibility.

**Both summary and full text.** Speech cannot be skimmed, so a digest exists — one short line per
message. But the capability that actually matters is **semantic random access**: describe a message
in your own words ("the one about the mac runner") and get *that* message back **in full**. That is
`POST /api/v1/channels/{id}/resolve`, and it is the reason this exists rather than a
text-to-speech bot.

**Messages and threads.** The channel view opens in **All** where the channel's chat service has
threads, and in **Main** — then the whole channel — where it has none, until you choose another
view. The choice belongs to that channel: another channel opens in its own choice or its default,
and coming back opens this one where you chose, across a reload too (see
[Reopening where you left off](#reopening-where-you-left-off)). When the backend finds threads,
three views are offered:

* **Main** shows channel messages, with a reply-count button below each thread root.
* **Threads** lists root-message previews and reply counts, ordered by most recent activity at the
  bottom. Opening the list starts at that end; scrolling upward loads older entries.
* **All** interleaves channel and thread messages chronologically. Colored **Thread** badges
  distinguish conversations; tapping a badge opens that thread.

A source added through Settings is always a separate entry in the channel picker, even when that
source identifies one upstream conversation rather than a whole room. The **Threads** control is a
different projection: it lists child conversations within the currently selected channel, and is
absent when the configured backend does not provide thread timelines or when the selected channel
is itself one conversation. It is not an alternate picker for added sources.

The selected channel or space is remembered when you reopen the app. Each window remembers its
own, so a reload brings a window back to the channel it was showing even when another window has
since chosen a different one; a new window opens on the channel chosen last in any window. If it
is no longer configured, the app selects the first available channel.

Tap a root's reply count or a thread-list entry to open its history. **Back**, or a swipe right,
returns to the view and scroll position you came from. The selected thread has its own history,
draft, and posting destination. Reply counts marked approximate come from the provider's estimate.
A thread nobody has answered yet — Google Chat and Slack name one for every message — is not a thread yet:
its tag is a plain grey **Thread**, drawn like **Reply** beside it with no thread colour, it gets no
reply-count button, and it is not offered in the picker. Tapping it **starts the thread**: the thread
opens with only that message in it (on the right, in two columns) and its message box focused, and
the first reply you send from there is posted into the message's thread. The first reply — yours,
or anyone's arriving on a read or the live stream — turns the tag into the thread's coloured
**Thread(1)** without a reload. A thread can only be started where the chat service has named one for
the message: every message on Google Chat and on Slack (whose thread is the message's own `ts`; see
[Slack](#slack)), a Discord thread somebody made that nobody has written in yet. Discord threads are
created in Discord, so a Discord message heading none has no tag. On a read-only channel the grey tag
is not shown, since nothing could be posted to start the thread.

**Reply answers a message where it is.** Replying to a thread message posts in that thread, from
any view, including **All**. Replying to a main-channel message posts in the main channel as a reply
to it: a quote on Google Chat, a reply link on Discord. (Slack has neither, so its backend posts
such a reply in the message's thread; see [Slack](#slack).) Where the chat service has already named
an empty thread for the message, as Google Chat does, the reply screen also offers **Start a new
thread from this message**, unticked each time it opens; ticked, the reply is posted into that
message's thread as its first reply, the way the grey **Thread** starts one. It is offered exactly
where that message's tag is the grey **Thread**: once the page holds a reply in the thread, from a
read or the live stream, the tag is **Thread(1)** and Reply has no new thread to offer. That holds
while the reply screen is open, too: a reply that arrives in the thread takes the box away. An
unticked reply then still goes to the main channel; a ticked one still goes into the message's
thread, now as a reply in it, and the title bar names that thread and the note under the box says
so. A draft put down with the box ticked has it ticked again when you come back to the same message,
until the page is reloaded; if the thread has been answered meanwhile, the draft is still bound for
that thread, and says so, rather than going to the main channel. After **Send** you are back where
you pressed Reply, and the message's tag is already its thread's **Thread(1)**. Slack does not get
that box: its reply already starts the message's thread, which the server says with
`replies_in_thread` in `/api/v1/client-config`. Nor does a Discord message that heads no thread:
Discord's threads are channels of their own, which vibe-talk does not create. A Discord message that
heads a thread somebody made in Discord, with nobody's reply in it yet, does get the box, as it gets
the grey **Thread**. A message that arrived on the live stream
without saying which thread it is in, and that no read has placed since, is placed by one read of
**All** before the reply is posted; if that read fails, nothing is posted and the reply keeps its
text for **Retry**. Until it is placed, whatever you send next in that channel waits for it.

**A reply you send marks what it answers Done.** Once the chat service has taken the reply, the
message you answered is marked Done exactly as its **Done** button would mark it: on the server, at
once on screen in both columns, with the **Undo** chip to take it back, and with the bookmark line
sliding on if it was right above that message. Its button then reads **Unarchive**, **Hide read**
leaves it out, **Collapse read** folds it, and a thread's count of unread replies on Main stops
counting it. Only that message is marked, never the rest of its thread. Opening Reply marks nothing,
and nor does a send that fails or is only partly posted; the **Retry** that sends it does. A message
already Done is left as it is, and the **Undo** chip keeps offering the last Done. A reply that
finishes sending after you have moved to another channel marks its message Done on the server only,
with no **Undo** here. `#256 reply-marks-read`.

**The reply screen says where the reply goes, and shows what came before.** Its title bar names
the thread the reply posts into as the thread picker and the heading over a thread name it (age and
replies, then the name, which gets a line of its own so a phone does not cut it away), or **Main**
and the channel's name, and it follows the **Start a new thread** box.
Above the message being answered are the messages before it in its thread, or on the main channel,
oldest at the top, drawn as the channel draws them but with nothing on them to tap. The screen opens
at the message, so they are a scroll up. The newest twelve the app holds come first; **Load earlier
messages** shows the rest, and then reads earlier pages of that thread, or of Main, from the server.

**A reply points at what it answers.** A message that is a reply is set in from the left, with an
arrow beside its box. When the message it answers is the one directly above it, the arrow leaves
the top of the box's left side and reaches up until it touches that message; when the message is
anywhere else, the arrow leaves the middle of the left side and slants away up to the left. Tap the
arrow to go to the message it answers, which is brought to the top of the list and lit for a
moment: in the view on screen when it is shown there, and otherwise in **All**, which the channel is
then kept in as if you had picked it. A message not loaded yet is looked for a few older pages back,
with progress in the status line; one further back than that, or no longer in the channel's history,
is said so there. While the reply is off screen, **Back to reply** returns to it.

**A message says how many replies it has.** Its **Reply** reads **Reply (1)**, **Reply (2)** once
replies to it are loaded, in any view, yours included from the moment you press Send, so an answered
message says so on the button that would answer it again; a screen reader hears "Reply, 2 replies
already". Replies that point at no message, such as a thread's, are counted by **Thread(N)** and
**N replies** instead, so on Slack, whose only reply is a thread reply, Reply stays **Reply**. When a
reply is not the message directly below, the message it answers has a small blue circle on its
lower-left corner, the other end of that reply's arrow. Tap it to go to the reply, brought to the top
of the list and lit as an arrow's tap brings what it answers, in **All** when the reply is in another
part of the channel. With several replies, each tap goes on to the next in time order, starting with
the first that is not directly below. The circle is not drawn for a reply directly below, whose arrow
already meets the message, nor in a gathered conversation, nor in the Pinned list.

**A message's replies can be gathered under it.** **N replies** under a message moves its loaded
replies, and the replies to those, all the way down, in time order, to sit directly beneath it in
**All**, joined to it by one line, while everything else stays where it was and the message stays
where it was on the screen. Tapping a reply's arrow twice does the same for the whole conversation
the reply is in, under the first message of it that is loaded; one tap still goes to the message
it answers. Nothing is read or archived by it. The **×** beside the line, or the pressed **N
replies**, puts them back in time order, as changing view or channel, or filtering from the search
bar, does. **Thread(N)** opens a message's thread.

**On Main, a message whose thread holds unread replies says how many.** Beside its **N replies**, a
chip in the accent says **M unread**, and a tap on it opens the thread at the first of them. Unread is
what the read modes call unread: not Done, not your own words, not a placeholder read automatically.
Such a message stays in the list under **Hide read**, and is never folded into a run under **Collapse
read**, although it is still drawn as read itself. The count follows replies as they arrive, as they
are marked Done here or on another device — Main's own refresh says which, with no read of All or
the thread — and as you answer in the thread, your own words counting as read and the reply you
answered marked Done. It counts the replies
the page holds — from **All**, from threads you have opened, and from the live stream — with their
Done state. While you stay on Main only Main is read, and Slack's live stream carries no thread reply
at all, so an answer can exist before the page has it: Main's refresh still reports each message's
reply count, and replies counted there that the page has not brought, posted since it last read the
thread or All, count as possibly unread. The chip then says **M+ unread** beside what is known, or **N
new** when nothing the page holds is unread, and the message is kept under both read modes until a
read of the thread or of All brings those replies. A tap reads the thread: it is drawn at once from
what the page holds — the rows of an earlier visit, or the copy saved on the device — and the read
behind them brings the reply and lands on it. Replies older than anything read of the thread are not
counted either way: **M+ unread** means older ones may not be loaded yet, and a thread with none it
knows of unread or new says nothing. Discord's own threads give only an estimated count, so there
nothing is inferred from it. `#222 unread-replies`.

**All is read behind Main when Main's counts call for it.** When a refresh of Main finds messages
whose reply counts are ahead of the replies the page holds — always so when Main is opened cold, on a
device that never read All, where every message with replies would otherwise say **N new** until All
happened to be opened — the page reads All's newest page once, behind Main: no loading line, nothing
waits on it, and a view switch is drawn from what is held as ever. The counts are then exact for
every thread All brings the replies of — on Google Chat all of them, on Slack at most ten (below). It
is not free: on Slack one read of All is a few of the chat service's calls for the channel's history,
plus one for each thread with new replies since the server last read it (a thread it has not read
in fifteen minutes is read in full); on Google Chat it is one more page. So it happens
at most once every five minutes per channel, counted from the last read of All of any kind, including
one saved on the device; only for news — a message already seen at the same count by the last such
read does not ask again; and never while the page is hidden. A read that fails waits out the same
five minutes. Discord's own threads never ask, having no exact counts. On a Slack channel where agents
keep answering while you sit on Main, that is a read about every five minutes: some 130 of Slack's
calls an hour on top of Main's own refresh.

**Slack's All brings the replies of at most ten threads per page**, the ten most recently active
among its messages, and says so in its notice. A thread past those ten is on the page with its count
but none of its replies, which says nothing about them: the replies the page already holds for it
stay held, counted and drawn, and its count says **N new** for any it has never held — on a device
that never read it, such as a fresh one, that is every reply it has. Nothing brings those replies
while nothing new happens in the thread; opening it reads it (it is never drawn from All with its
replies missing), and marking the message itself Done on Main — its own **Done**, or **Mark read
through here** — says you have dealt with it: replies no read has placed are then taken as read, and
only a reply posted after that counts as new. **Undo**, or putting the message back, takes that back.
Done in All, in a thread or in the Pinned list, where nothing says the message has new replies,
settles none. Neither Main's refresh nor a step back in All bringing the message alone counts its
replies as old; a step back that brings, without its replies, an old thread All's newest page did
not pass over counts them as old, not new.

**What Done on a message does not settle, and why.** Done on a message is Done on that message, and
never stands for a reply that may be unread. A reply the page holds keeps its own Done state, which
other devices share, so **1 unread** stays until that reply is marked. A reply counted as posted
since a read that placed the thread stays **1 new**, so Done through a list cannot hide an answer
nothing has brought; so does an unread reply cut from the device's copy (below). Only replies the
page cannot place at all — read long ago or new, it cannot tell — are taken as you say.

Known limits of the counts (`#222 unread-replies`):
- A reply the page never held that is marked Done on another device still counts as new on its
  message until a read of All or of the thread brings it: Main's refresh names Done replies by id,
  and the page cannot tell which of a thread's unknown replies those are.
- Your own reply posted from another client counts as **1 new** until such a read brings it — the
  read behind Main above, at most five minutes later, or a tap.
- On Slack a refresh of Main between full reads may not carry a message's new reply count; the next
  full refresh, at most forty-five seconds later, does.
- A Slack thread past the ten All brings replies for, whose replies this device has never held, says
  **N new** until it is opened or its message is marked Done, or until new activity puts it among the
  ten and a read of All brings its replies. On a fresh device, a busy channel can show many such
  messages under **Hide read** at first; **Mark read through here** on Main clears them at once. Done
  there is this device's: another device that never held those replies still counts them.
- The device keeps the newest 120 messages of a channel. After a reload, a read reply cut from that
  copy counts as old, and an unread one as **new**, so its message stays under **Hide read** until a
  read of the thread or of All brings the reply back.
- A copy saved on the device by an earlier version of the page is placed afresh once: All is read
  behind Main, and a message whose replies Done had settled may say **N new** again until it is
  marked Done once more.

**Reactions show under each message.** Where the chat service reports them, a message's emoji
reactions appear as a row of small chips under its text, each the emoji and how many reacted with it
— a chat bridge's 👀 when it has a message and ✅ once the message reached the agent, for example.
They are read-only: who reacted is not shown, and nothing here reacts. A custom emoji shows its name
between colons. A post combined from several messages shows one row of chips, each emoji counted
over all of them. Reactions reach the page with the next read, so they can trail the message by a
refresh. Discord and a compatible bridge report them; the Slack backend does not yet.

**An agent's own heading moves into the header.** Agents posting through one shared account often
open a message with who they are, a title and a time — `[build-bot coordinator] Backup check, 10:52
PM ET. All shards are synced.` That first line is drawn in the row's header instead: the tag in quiet
italics, the title in bold and the time after an `@`, beside the row's own time where a wide screen
has room, and as the row's first line under the buttons where it does not, as on a phone. Nothing is
dropped either way, and copying, searching and reading aloud use the message as written. **Rich
headers** in Settings turns it off, and every first line is drawn as written. See
[Rich headers](#rich-headers).

**A normal text box at the bottom.** Scroll below the newest message to write to the channel or
selected thread. **Send** posts without requiring a message to reply to. Drafts are kept separately
for each channel and thread. This composer scrolls with history.

**Sending responds immediately.** Normal messages and replies appear locally with a sending
indicator, leaving the composer ready for the next message. Confirmation replaces that indicator
with the delivered message. A failed or interrupted send keeps its text and offers a manual retry;
partially sent messages retain the remaining text. When delivery is unconfirmed, check the history
before retrying because the chat service may have received the message. Reopening the app restores
unfinished sends without sending them again automatically.

**Messages leave in the order you wrote them** (`#277 send-order`). Messages to the same place — the
channel, or one thread in it — go one at a time, in the order they were written. While an earlier
one is still sending, waiting to go again, or not sent, the ones after it wait, and each says what
it is waiting behind and why ("Waiting to send… “A” goes first, and goes again in 6 s."). When the
earlier one is sent they follow in order; **Dismiss** or **Edit** on it lets the next one go. A
message that is waiting can itself be edited or dismissed, but not sent ahead of the one in front of
it. A thread does not wait for the channel or for another thread, nor one channel for another. A
reply to a message that came in live, whose thread is not known yet, can still go anywhere in its
channel, so everything written after it there waits until it has been placed and sent, dismissed or
edited, even when finding out where it goes failed; a read that failed for a moment is tried again
on its own. Where the chat service can recognise a repeated request, a send whose answer was lost
goes again on its own, under the same request id, after 3, 6 and 12 seconds and then every 20
seconds, six times in all, each wait counted from when the failed try started, so a try a slow
service took half a minute to fail goes again at once; then it waits for **Retry**. Reopening the
app keeps the order: a message that was waiting still waits, and goes once the one ahead of it is
retried, dismissed or edited. The server keeps the same order: it posts to each destination one at a
time, in the order the posts reached it, the parts of a long post together. A post holds its
destination until the chat service answers it, even after the app has stopped waiting for it, so a
retry cannot overtake the attempt it repeats — up to the server's own deadline for one request to
the chat service (`request_timeout_seconds`, 20 seconds by default). Past that deadline the server
stops waiting, and the next post can reach a slow bridge that is still working on the first.

**Taking a failed send back** (`#193 compose-draft-loss`). What you type in the channel's box, a
thread's, Reply and the call's box is saved as you type and comes back after a reload or a sign-in.
A send that did not go through stays in the outbox with these actions:

- **Retry** sends it again, as before.
- **Edit** puts the unsent text back in the box it came from: the channel's or the thread's, or Reply
  on the message it answered, with "Start a new thread" ticked if that is how it was sent. The send
  leaves the outbox. If that box already holds a draft, the text goes under it after a blank line,
  and the box says so. Nothing is overwritten. Edit on a thread's message in All opens the thread.
  Edit is offered only on a send that was refused or never left the device. A send whose delivery
  is unconfirmed may already be in the channel, so it offers Retry and Dismiss instead: where the
  chat service supports it, Retry repeats the same request and cannot post the message twice, and
  sending the words again from the box would be a new message. If the browser cannot save the text
  as a draft, Edit leaves the send in the outbox and says so.
- **Dismiss** takes the send out of the outbox. For 15 seconds an **Undo dismiss** chip naming the
  message puts it back as it was. After that the message is still kept on this device for 7 days
  (the newest 10) under **Settings → Recently dismissed**. **Put back** there returns it to
  its channel's outbox, and **Forget these now** clears the list. If the browser cannot keep that
  copy, the page says Undo works only until it is reloaded.

A message with a saved reply draft shows **Draft**, with a pencil, just left of its Reply. This
works in Main, All, a thread and either column. Tap it to reopen Reply with the draft. It goes away
when the draft is sent or emptied.

**Drafts stay on the device you typed them on.** They are kept in this browser's storage and are
not synced, so a draft started on the phone is not on the laptop. Syncing drafts between devices
is not supported yet.

**A long message stays one message where it can.** A message is cut into parts only when it is
longer than the chat service accepts in one message, and each service is measured its own way:
Discord takes 2,000 characters, Google Chat 32,000 bytes, and Slack 40,000 characters, with a bridge
in front of a service taking what its operator configured (see `max_post_chars` and `max_post_bytes`
under [Configuration](#configuration)). When a message to the channel itself still has to be cut,
Google Chat and Slack post the first part in the channel and the rest as replies in that part's
thread; Discord posts the parts one after another, and only the first answers the message you
replied to. A message sent into a thread keeps every part in that thread. If a part fails, the
unsent rest is retried into the same thread (`#263 post-split-per-provider`).

## Installing the reader

Serve vibe-talk over HTTPS, open `/voice` in Chrome on Android, and choose **Install app** from the
browser menu. The installed app uses vibe-talk's speech-bubble icon and opens in a standalone
window. If an older shortcut still has a blank or generic icon, remove it and install again.

The manifest asks browsers that support `display_override` (including Chrome Android) for
`standalone`, while retaining `display: browser` as the standards-defined fallback. Safari on iOS
currently ignores `display_override`, and this page deliberately does not opt into Apple's
standalone meta mode: it therefore remains in browser mode, preserving the established microphone
permission fallback instead of risking repeated prompts after app switches.

Installation does not add offline access, and there is no service worker. What makes a launch
fast is the browser's ordinary HTTP cache, holding only the page's public files. Every script,
stylesheet and icon the page names carries the hash of its own bytes (`/voice.js?v=<hash>`) and is
`immutable` under that URL, so the browser never asks for it again. The two pages and the manifest
are revalidated on every load (`no-cache` with an ETag, answered `304` when unchanged). Launching
the installed app therefore costs one small request for the page when nothing was deployed, and
after a deploy only the files that changed. The plain URLs (`/voice.js`) still work, revalidated,
for an app installed before this. The server applies `Cache-Control: no-store` centrally to all
`/api/` and `/mcp` responses, including errors. Transcripts, minted session URLs, and API responses
are never placed in Cache Storage or any other browser-managed HTTP cache. The installed app still
requires the server to be reachable to load at all.

To ask Chrome's own installability engine about the checked-out page, run:

```sh
make -C vibe-talk pwa-installability
```

The check uses Playwright's Chromium by default and prints its version. Set
`VIBE_TALK_CHROMIUM=/path/to/chrome` to use a particular Chrome build, or run
`python3 vibe-talk/tests/pwa_installability.py --url https://your-host/voice` to inspect a deployed
origin. A green result means CDP reports no manifest or installability errors under an Android
viewport; it is not proof that a physical phone displayed the menu item or launched the app.

### Saved messages on this device

Opening the channel view used to mean a blank list until the server answered. `/voice` now keeps a
bounded snapshot of the channel rows it has already shown, in `localStorage` under
`vibe-talk.voice.message-cache`, and draws it immediately on the next visit:

- **Drawn before the network.** The rows appear as soon as the page loads, with a pill over the top
  of the list saying `Saved 14:02 · refreshing…`. It disappears once the refresh lands.
- **One read to catch up.** A cold start makes one newest-page read, for the channel and view on
  screen only, and merges it in without duplicating rows. Rows the server no longer returns are
  dropped. It does not refetch every saved channel or walk back through history. After that, the
  live stream and the regular poll keep the view current.
- **Views are local.** The snapshot keeps one store per channel. Switching among Main, All and a
  thread already read is redrawn from that store without a request.
- **Says when it is current.** After a read the pill says `Updated HH:MM`, or `Live · updated
  HH:MM` while the live stream is attached, with `· refreshing…` while a read is in flight. A live
  message moves the time forward.
- **Honest when stale.** If the refresh cannot reach the server, the rows stay and the pill says
  `Offline · showing messages saved …`. A server error says `Refresh failed · …`.
- **Bounded.** At most 120 messages and 120 thread cards per channel, 12 channels (the least
  recently used goes first) and about 600,000 characters in total. When storage is full the
  snapshot shrinks, and unsent messages in the outbox always win the space. A damaged entry is
  discarded rather than drawn.
- **Tied to the token.** The snapshot records a fingerprint of the token that read it. Signing
  out, saving a different token (on either page, or in another tab), or a `401`, or a `403` on a
  read, removes it. A channel dropped from the server's configuration is pruned.
- **Not a cursor store.** Provider paging cursors expire, so they are never saved. Scrolling above
  the saved rows waits for the first live read.

This is chat text at rest in the browser profile. Anyone who can open that profile can read the
most recent rows until the token is changed or you sign out. It is not an offline mode: without an
app-shell service worker the page itself cannot load with no network, only keep what it has.

### Reopening where you left off

A phone closes an installed app whenever it needs the memory, and reopening it used to land on the
call view, in Main, at the newest message. `/voice` now keeps one small record in `localStorage`
under `vibe-talk.voice.ui-state` and reopens on it:

- **The call or the channel**, whichever was up. Settings, Help, Reply and the Threads screen are
  not reopened; a reply's draft is kept, as before.
- **Main, All or the thread** that was open, in the channel that was selected, with the thread's
  name in its heading.
- **The view you chose in every other channel.** Main, All or a thread chosen in a channel is that
  channel's, so changing to it — in the same session or after a reopen — opens it there, with the
  one read of that view a channel change makes. All is kept when you pick it, so a channel you put
  back in All stays there whatever the default is. A channel you have not chosen a view in opens in
  All where its chat service has threads and in Main where it has none. The 50 channels most
  recently on screen keep their choice, and fewer when long thread roots would make the record
  large; the least recently used give theirs up first.
- **The read mode**: Show read, Collapse read or Hide read.
- **The message you were reading**, at the same height on the screen — the message, not a pixel
  offset. If you were on the newest message, or yours is no longer on the device, the newest.
- **One read.** The reopened view is drawn from the saved messages first, and the single newest-page
  read of a cold start is that view's. It waits until the server has accepted the token and the live
  stream has attached, or three seconds have passed without it, so a refused token is reported as
  such, and the stream's replay of what changed while the app was closed costs no second read.
- **Checked on the way back.** A channel no longer listed, a thread the device no longer knows of, or
  a chat service that no longer has threads falls back to the default, and a damaged record is
  discarded. A thread is known while the device keeps that channel's saved messages; once it has
  fallen back, the channel stays in its default until you choose again. The record is tied to the
  token like the snapshot, and goes with it; a channel the server stops listing loses its choice
  when it loses its saved messages. A record from before each channel kept its own view is carried
  forward: the view it held, if you had chosen it, becomes that channel's choice.
- **Each window its own** (`#265 window-channel-memory`). `localStorage` is shared by every window
  of the app, so with two windows open a reload used to come back on whatever the window that wrote
  last had shown. The selected channel and this record are now also kept in each window's own
  `sessionStorage`, which survives a reload of that window and is read first: two windows left on
  two channels, or on one channel in two views, each come back where they were. A new window has
  nothing of its own and opens from the shared copy, on the channel chosen last in any window;
  reloading a window does not count as choosing. The read mode is a device-wide preference and
  comes from the shared copy, and a view chosen in another window still applies in a channel this
  window has not chosen one in. What the right column shows in two columns is not kept across a
  reload, as before.

`make -C vibe-talk offline-cache-browser` walks that whole sequence in a real Chromium at an
Android phone's size, against a loopback fake API. Pass `SCREENSHOTS=/tmp/shots` to keep one PNG
per step.

## Reading messages with a device voice

To use **Read** in the message view without an ElevenLabs account or conversational agent, add
this to the server configuration and restart it:

```toml
[read_aloud]
backend = "browser"
```

Reload the phone page, open the message view, press **Read**, and tap a message. Tap that message
again to stop it, or tap another to switch. **Stop** leaves reading mode; **Pace** changes the
speed. A combined row is read in order, and messages are marked read only after playback finishes.
This works independently of the conversational voice mode and per-message summarisation.

Chrome on Android exposes the installed Android text-to-speech service through the Web Speech
API. The page selects a voice the browser reports as local, loads voices as they become available,
and starts playback from a tap. If no voice is available, install or download a voice in Android's
text-to-speech settings and try again. Embedded Android WebViews do not support this API; open
the page in Chrome instead. Keep the page visible while listening: background and locked-screen
playback depend on the browser and are not guaranteed.

The app makes no ElevenLabs speech request in browser mode. The phone's configured speech engine
controls its own processing: Chromium's `localService` flag does not prove that every Android
engine stays offline. Choose a downloaded offline voice and check it in airplane mode when that
distinction matters. Browser automation verifies the interaction with a simulated speech engine;
the opt-in physical-device check below verifies the audible output itself.

### Checking the audio an Android device actually emits

`scripts/android-device-speech.py` closes the gap between a JavaScript call to `speak()` and sound
that can be understood. It drives the real served `/voice` page in Android Chrome over ADB, selects
device speech, records Android's rendered playback, converts it to mono PCM, rejects silence and
clipping, and passes the recording to an STT adapter. The run fails if the detected language does
not match Chrome's language or if the recognised words do not match the selected message.

The device must have USB debugging enabled and Chrome open. Python Playwright, ADB and ffmpeg are
the host dependencies. The STT adapter is an executable outside this repository: it receives the
WAV path as its only argument and prints JSON containing `text`, `language`, and optionally numeric
`confidence`. This keeps local models, service credentials, and deployment-specific routing out of
the reusable source tree.

```sh
cd vibe-talk
VIBE_TALK_WRITE_TOKEN=... scripts/android-device-speech.py \
  --url https://vibe-talk.example.invalid/voice \
  --serial ANDROID_SERIAL \
  --transcriber /path/to/stt-adapter
```

The token is accepted only through the environment and is placed into that Chrome origin through
the debugging connection; it is never put on the command line or printed. By default the harness
uses Android `screenrecord` only when its help explicitly advertises internal playback capture. On
other devices, `--capture-adapter PATH` supplies a recorder; it is called as
`PATH DURATION_SECONDS OUTPUT_PATH` and may, for example, record the phone's speaker through a host
microphone. `--channel`, `--message-id`, and `--message-index` choose a known row. Run `--help` for
all defaults and applicability.

Artifacts—including the source text and STT result—go under the gitignored
`debug/android-device-speech/` directory by default. `--signal-only` is useful for diagnosing the
capture route, but it deliberately does not count as the language regression. The evaluator's
silence, clean-signal, wrong-language, unrelated-words, and malformed-STT negative controls run in
the ordinary `make validate` suite through `--self-test` and need no device.

Omitting `[read_aloud]`, or setting `backend = "elevenlabs"`, preserves server-generated ElevenLabs
audio and its existing voice configuration. A deployment whose conversational backend speaks the
public `vibe-talk-v1` WebSocket protocol can set `backend = "conversation"`; the server sends the
message as a text turn, collects the agent's 24 kHz PCM response, and returns WAV audio. When a
server-audio backend is available, the channel bar offers an instant device/agent selector.
`read_aloud.websocket_url` may name a server-only route to that same agent when the browser-facing
conversation URL goes through a different ingress.

The page plays a prepared read **as it arrives**. It fetches the audio itself and schedules the
PCM on a Web Audio context, because an `<audio src>` element buffers a streamed WAV by bytes —
Chromium reads about 225 KB of samples, 4.7 seconds of 24 kHz speech, before it starts. Tapping
another message or leaving Read aborts the fetch, and closing that response is what tells the
server to interrupt the agent. A browser without streamed `fetch` bodies or Web Audio falls back to
`<audio>`. `make -C vibe-talk read-aloud-browser` times tap to audible in a real Chromium against
audio streamed at speaking pace, with that `<audio>` fallback as its negative control.

A tap never waits for the whole screen to be prepared (`#191 read-aloud-first-lag`). Turning Read
on prepares every visible message in the background. A tap on a row that is not ready yet sends a
prepare request for that row alone and starts reading it, and the rest of the screen keeps
preparing behind it.

On the server, preparation first looks at messages fetched for the page in the last 30 seconds,
kept in memory (`src/read_ahead.rs`, apart from the voice catch-up's windows in `src/recent.rs`).
It fetches only what is missing:

- outside a thread, one window per channel, shared by preparations that miss at the same time
  (a preparation joins another's fetch only when that window was at least as wide as its own);
- in a thread, the missing messages, four at a time per preparation and eight across all of them.

An edit or deletion an adapter pushes, a deletion a timeline delta reports, and the storage purge
all drop the copy in memory at once. A fetch that was already in flight when that happened keeps
none of what it was told about, so it cannot put the old text back.

The provider warm-up starts at the top of a preparation and is not awaited. Only one runs at a
time, and it is given up after 60 seconds. Each preparation logs one content-free
`read-aloud preparation` line with its phases (`lookup_ms`, `fetch_ms`, `normalize_ms`,
`preparation_ms`, and whether it started the warm-up). The warm-up logs its own `warm_up_ms` when
it ends: at INFO when it worked, and at WARN with the provider's error `code` when it failed or
timed out. A read's audio opens with silence, which `playing` already counts as
audible. So where the page plays through Web Audio, its playback report also carries
`tap_to_speech_ms`: the time to the first sample loud enough to be speech.
`GET /api/v1/client-config` reports the selected backend and playback mode. Rust handlers depend on
`speech::SpeechProvider`; vendor credentials and API details stay in its adapter.

## Setting it up

**If you are setting this up for the first time, use [`QUICKSTART.md`](QUICKSTART.md).** It runs
the six steps — Discord bot, tokens, run it, expose it (optional), ElevenLabs agent, first
conversation — in order, with the exact commands and no gaps to fill in from here. This file is
the reference behind it: what every route does, what the security model is, and where it stops.

Two things are worth knowing before you begin, because they shape everything else:

* **It assumes no infrastructure.** One machine that can run a static binary, or a container
  runtime, is the whole requirement. There is no database to provision, no queue, no reverse
  proxy, no cloud account and no domain. See **From zero** below.
* **What you have to go and get is a short list, and it is the slow part.** Two vendor dashboards
  hand out five values between them and one of those is shown exactly once. See **What you must
  supply** below, which is written as a walkthrough rather than a table for that reason.

## Status: what works, what is stubbed

| Piece | State |
|---|---|
| HTTP server, routing, JSON API | **works** |
| Config file + environment overrides, with validation | **works** |
| Bearer-token auth, read scope vs write scope | **works** |
| **Per-request access log** | **works.** One INFO line per HTTP request, per MCP JSON-RPC message, and per tool call. No credential ever, no channel text above DEBUG. |
| Discord read + post behind a trait | **written, unit-tested; never run against live Discord** |
| In-memory Discord for tests and `--fake-discord` | **works** |
| Digest (`/digest`) | **works**, extractive and deterministic (no model call). This is the whole-channel digest, not the per-message summary below. |
| **Per-message summaries and thread names, from one Messages API call** (`[summaries.llm]`) | **written, tested against a local mock Messages API only; never yet run against a live model provider.** One plain HTTP request per summary or thread name, with no agent harness and no tools, in the Anthropic API's dialect or Google Vertex's. See [Summaries](#summaries). |
| Per-message summaries, from the ElevenLabs agent | **written, unit- and integration-tested offline, NEVER RUN AGAINST LIVE ELEVENLABS — and it is what runs when `[summaries.llm]` is absent.** Asks the configured conversational agent in text over its own WebSocket, on a pooled conversation recycled every eight summaries. The frames were read out of the vendor's SDK; the offline tests drive the real socket client against this repository's own mock, which was written from the same reading, so the two agree with each other and not yet with ElevenLabs. The extractive truncating summariser that used to be the default has been **deleted**, so there is nothing to fall back to: a deployment without ElevenLabs credentials still starts, and shows every long message as a **failed** summary — a red row — instead of handing you its opening lines and calling that a summary. |
| Semantic random access (`resolve`) | **works**, lexical ranking behind a `Ranker` trait |
| Web app: text tab, digest, find-a-message, local speech | **works** |
| Device speech for the message view's Read control | **prototype.** Uses browser-reported local voices without ElevenLabs credentials. Playback, cancellation, and failures have automated browser coverage; physical-phone audio and background playback remain device checks. |
| Main, All, a selected thread from the bar's picker, and bottom composer | **works.** Native thread endpoints have loopback HTTP tests; a compatible Google Chat bridge has been exercised with real read-only traffic. Browser interaction and layout were checked at two phone sizes. Posting tests use local fakes. |
| **MCP over Streamable HTTP at `/mcp`** | **works.** Bearer-authenticated, stateless, eight tools, tested end to end. Never yet driven by a real ElevenLabs agent. |
| ElevenLabs voice agent | **reachable, and currently NOT invoking tools.** A real agent has now been driven headlessly (`scripts/run.sh --smoke-agent`): the signed URL mints, the conversation opens, the agent answers — and it calls no tool, saying its tools "appear to be out of date". In the same conversation ElevenLabs reports our MCP server connected with all five tools visible, so the fault is in the agent's own configuration rather than in this server. |
| **Signed conversation URLs at `/api/v1/signed-url`** | **works against a fake, unverified against live ElevenLabs.** Mints a short-lived signed URL for an agent that has "Enable Authentication" turned on, and `/voice` is a dependency-free page that uses one. Tested end to end against an in-memory ElevenLabs that refuses a wrong key and an unknown agent, and against a loopback HTTP server that proves the account key travels in a header. |
| Slow path (ask a coding agent for detail) | **seam only.** The route exists and answers HTTP 501 with an explanation. |
| TLS | **not here.** Terminate in front, with whatever you already use — Caddy, nginx, any tunnel, a cloud load balancer — or do not expose it at all. One worked example is below; it is an example, not a requirement. |
| Agent smoke test (`scripts/smoke-agent.py`, `run.sh --smoke-agent`) | **works, and it goes red on the real failure.** Holds a real conversation and fails unless a tool invocation appears in this server's own access log. Manual and opt-in: it costs vendor minutes, so it is in no suite and no CI. |
| Diagnostics route (`GET /api/v1/diagnostics`) | **written, tested against in-memory fakes only.** Re-runs the startup checks on demand and answers a structured report with a remedy on every failure. The Discord and ElevenLabs halves have never met a live vendor. |
| Deployment check (`scripts/verify-deployment.sh`) | **works.** One command, pass/fail, exits non-zero and names the failing check. Runs in CI against the container on every push, including a negative control that requires it to go red. |

Honest summary: everything except the two vendor-facing halves — live Discord and ElevenLabs — is
implemented and tested. Those two are exactly the parts that cannot be tested without credentials.

## What you must supply

Nothing is committed and nothing is hardcoded, so every value below is one you go and get. They
are listed in the order you need them: items 1–4 are what it takes to start the server at all, and
items 5–7 are the voice half, which the Discord half runs perfectly well without.

Have somewhere to paste things before you begin: two of them — the bot token and the ElevenLabs
API key — are shown exactly once and cannot be read back afterwards.

### 1. A Discord bot token

Use a **second, dedicated bot**, separate from whatever your coding agents post with. This one is
held by a server that may end up reachable from the internet, and the blast radius of the two
should not be the same.

1. <https://discord.com/developers/applications> → **New Application**. Name it anything.
2. **Bot** tab → **Reset Token** → **Copy**. That string is `discord.bot_token` /
   `VIBE_TALK_DISCORD_BOT_TOKEN`. It is shown once; paste it somewhere before navigating away.
3. **Bot** tab → **Privileged Gateway Intents** → turn on **MESSAGE CONTENT INTENT** → **Save
   Changes**. See the warning below; this is the one that costs an hour.
4. **OAuth2** → **URL Generator** → scope **`bot`**, then bot permissions **View Channels** and
   **Read Message History** — and **Send Messages** *only* if you want the bridge to be able to
   post. Open the generated URL, pick your server, authorize.

> **Adding the bot to your server is not the same as adding it to the channel.** Authorizing
> the OAuth2 invite puts the bot in the *server*; a **private** channel additionally needs the
> bot, or a role it has, added in that channel's own **Edit Channel → Permissions**. Missing
> that second step is the most common way this server ends up configured for a channel it
> cannot read. The [startup channel probe](#the-startup-channel-probe) now catches it at
> startup instead of letting it surface later as an empty digest.

> **Turn on MESSAGE CONTENT INTENT** while you are in the Developer Portal: **Bot** tab →
> **Privileged Gateway Intents**. It is off by default and it is the highest-cost thing to get
> wrong here, because it does not fail — a bot without it is handed messages whose `content` is
> **empty**, so the channel is found, the message count is right, and every digest line is
> blank, with no error anywhere. It reads as a broken summarizer or a broken server.
>
> Scope, stated honestly: this server reads over the Discord **REST** API, and the documented
> blank-content behaviour is on the **gateway** event path; we have never run against live
> Discord and cannot tell you from experience whether REST is affected on your account.
> Enabling it is one click and removes the question.

### 2. The channel snowflake ids

In the Discord app: **User Settings → Advanced → Developer Mode** on. Then right-click each
channel you want reachable → **Copy Channel ID**. They are 17–20 digit numbers.

Give each one a **label you can say out loud** — it is what you will use in a sentence to a voice
agent, so `lead team` beats `#eng-agents-prod-2`. Labels and ids go in `[[channels]]` or in
`VIBE_TALK_CHANNELS` as `id:label:rw` / `id:label:ro`.

### 3. Your own Discord user id

`discord.owner_user_id` / `VIBE_TALK_DISCORD_OWNER_USER_ID`. With Developer Mode already on,
right-click **your own name** on any message → **Copy User ID**.

**This one cannot be derived from anything the server holds**, which is why it is a setting at all.
The bot token's first segment *is* the bot's user id, so messages the bridge posts for you are
recognised for free — but a bot account has no relationship to the person reading the channel. Set
this and the messages you typed into Discord yourself are drawn as **yours**; leave it unset and
they arrive as some third party's. It is not a secret: a user id rides on every message that
account has ever sent. See **Why your own messages need a setting** below, and note that Settings
in the web app can assign it per account without a restart.

### 4. Two API tokens of your own

These are neither Discord's nor ElevenLabs'. They are what this server demands of *its* callers —
your phone, your voice agent — and they are the whole of what stands between the internet and your
Discord.

```sh
openssl rand -base64 33   # read token  — goes in the phone and in the voice agent
openssl rand -base64 33   # write token — the capability to post as your bot
```

They **must differ** and each must be **at least 24 characters**, or the server refuses to start
rather than coming up weaker than you meant.

### 5. An ElevenLabs API key — only for the voice half

<https://elevenlabs.io/app/settings/api-keys> → **Create API Key** → copy it. It is shown once.

It goes in `elevenlabs.api_key` / `VIBE_TALK_ELEVENLABS_API_KEY` and it is used for two things:
minting the short-lived signed URLs that open a conversation with an authenticated agent, and
reading a message aloud. It is an **account** secret that can spend money, so it never leaves the
server — it travels to ElevenLabs in a header, never in a URL, and is redacted out of every error.

### 6. An ElevenLabs agent id — only for the voice half

<https://elevenlabs.io/app/agents> → create an agent (a default template is fine; keep their
hosted LLM). Open it, and the id is in the dashboard URL — it starts `agent_`. It goes in
`elevenlabs.agent_id` / `VIBE_TALK_ELEVENLABS_AGENT_ID` and is public: it identifies a widget.

### 7. An ElevenLabs voice id — OPTIONAL, and most deployments should skip it

`elevenlabs.voice_id` is **optional and normally left unset**, and the reason is worth knowing
rather than guessing at. An agent and a voice are different objects in ElevenLabs' model: an agent
is a prompt, a model, tools *and* a voice, while the text-to-speech API takes a voice and knows
nothing about agents. With the ElevenLabs read-aloud backend and `voice_id` unset, read-aloud **borrows the configured agent's own
voice** — and the agent's delivery with it, its speed and stability — so a channel message is read
out by the same voice that talks to you, from the one thing you already configured.

Set it only when those two should deliberately **differ**. With neither a voice id nor an agent id
configured, the ElevenLabs backend refuses and names both, rather than falling back to whichever voice the
account happens to list first.

Copy `vibe-talk.example.toml` to `vibe-talk.toml` (gitignored) and fill it in, or pass everything
through the environment. The environment wins over the file, so a container can be given its
secrets without them ever touching a disk.

## From zero

This section assumes you have nothing: no cloud account, no domain, no reverse proxy, no
orchestrator. Three decisions, in order, and only the first is mandatory.

### 1. Run the server somewhere

The minimum is **one machine that can run one process and reach `discord.com` outbound**. There is
no database to provision — durable state, when you turn it on, is a single SQLite file — and
nothing about the deployment is tied to the host: a laptop, a Raspberry Pi, a spare VM and a small
cloud instance are all the same deployment, and moving between them is moving a config file.

Two ways to run it, and neither is preferred:

* **Cargo, on a machine you already have.** A Rust toolchain, `cargo run`, done. `web/` is compiled
  into the binary, so there is no asset directory to serve and no build step for the front end.
* **The container image.** `podman build` (or `docker build`) from `vibe-talk/Containerfile`. The
  image runs as a non-root user, contains no configuration, and fails immediately rather than
  starting with defaults if it is given no credentials.

Both are spelled out under **Running it** below, including the volume mount you need if you want
state to survive a rebuild. If you just want to look at the web app, `--fake-discord` needs no bot
and no network.

### 2. Expose it — OPTIONAL

**You do not have to.** A deployment that is only ever reached over your LAN, or only over
`localhost` through an SSH session, is a supported and complete deployment. The Discord half, the
web app, the digest, the scrollback and the deployment check all work with no ingress whatsoever.
Skip to step 3 if that is you.

If you do want to reach it from elsewhere, **any** mechanism is fine, because vibe-talk takes no
position on how bytes arrive. The actual requirement is short:

* **a URL a browser can load**, and
* **HTTPS if you want the microphone**, because browsers gate `getUserMedia` on a secure context.
  `localhost` also counts as a secure context, so a local-only setup needs no certificate at all.
* **vibe-talk terminates no TLS itself** and never will. It speaks plain HTTP and expects
  something in front of it if the front is public.

Pick whichever of these you already know, or already run:

| Way in | What it gives you | What it costs |
|---|---|---|
| **Nothing at all** — LAN or `localhost` | The whole application, microphone included over `localhost` | No remote access |
| **SSH reverse tunnel** (`ssh -R`) | A port on a host you already have | You manage TLS on that host, or use it over `localhost` |
| **Reverse proxy with a certificate** — Caddy, nginx, Traefik | HTTPS on a name you control | You need a public host and a domain |
| **A tunnel service** — Cloudflare Tunnel, ngrok, Tailscale Funnel and others | HTTPS with no inbound port open | A vendor account and a dependency on it |
| **A cloud load balancer** | HTTPS in front of a private instance | Cloud account, per-hour cost |
| **A VPN or overlay network** — WireGuard, Tailscale, ZeroTier | Reachable from your devices, exposed to nobody | Every client device must join the network |

**A worked example of one of them** — a Cloudflare Tunnel — is written out under
[Exposing it: one worked example](#exposing-it-one-worked-example) below. It is there because a
complete example is more useful than six partial ones, not because it is recommended over the rest.

Whatever you choose, bind the server to loopback (`VIBE_TALK_BIND=127.0.0.1:8080`, or
`-p 127.0.0.1:8080:8080` under a container runtime) when something else is fronting it, so the
only path in is the one you meant.

**One thing does need a public URL: a hosted voice agent.** ElevenLabs calls the MCP endpoint
machine-to-machine from its own infrastructure, so it must be able to reach your server. If you
are not wiring up a voice agent, that constraint does not apply to you.

### 3. Check it, from the outside

`scripts/verify-deployment.sh --url <wherever it is>` is the same set of checks wherever you point
it, which is the point — run it against `http://127.0.0.1:8080` first and against the public URL
second, and any disagreement is something your ingress did. See **Checking a deployment** below.

## Running it

Locally, without touching Discord at all:

```sh
cd vibe-talk
cargo run -- --config vibe-talk.toml --fake-discord
```

`--fake-discord` serves in-memory channels seeded with a dozen deliberately long-winded agent
messages, and logs a warning on every start. It is how you look at the web app without a bot.
The seed is verbose on purpose: two cheerful one-liners make the digest and the scrollback look
like they work when neither of them had anything to do.

Against real Discord — **note that this is very nearly first contact.** The Discord client is
unit-tested and the whole server is end-to-end tested against an in-memory fake. Exactly one path
has been exercised against `discord.com` itself: the startup probe was run with a deliberately
invalid bot token and Discord answered **401**, which the probe classified and reported correctly.
Nothing else — no authenticated read, no post, and none of the 403/404 classifications — has ever
reached the real API. If something breaks here it is the untested seam finally being tested, not
a regression:

```sh
cargo run -- --config vibe-talk.toml
```

Then open `http://<host>:8080/`, go to **Settings**, paste one of **this deployment's own** two
tokens into **vibe-talk API token**, and press Save. Not the bot token and not the ElevenLabs key:
neither is ever entered into the page. `VIBE_TALK_READ_TOKEN` is enough to read, search and
listen; `VIBE_TALK_WRITE_TOKEN` is what replying from the page needs, and what `/voice` needs to
start a conversation at all. `/api/v1/client-config` accepts both, so a read token loads the whole
interface and then fails at the first write, which is why both refusals now name the scope. Its
`token_scope` field (`read` or `write`) names the scope of the token that asked — nothing the caller
did not already hold — and the page uses it only to skip what a read token cannot have: with one,
Settings says stored conversations need the write-scope token rather than asking for them and
logging a 403. It is stored per browser **and per hostname**, so a second URL onto the same
deployment asks again.

### Podman

Build (context is the `vibe-talk` directory):

```sh
podman build -t vibe-talk:v0 -f vibe-talk/Containerfile vibe-talk
```

Run with everything from the environment — no configuration file in the image, no secret baked in:

```sh
podman run --rm --name vibe-talk -p 8080:8080 \
  -e VIBE_TALK_DISCORD_BOT_TOKEN='your-bot-token' \
  -e VIBE_TALK_READ_TOKEN='your-read-token' \
  -e VIBE_TALK_WRITE_TOKEN='your-write-token' \
  -e VIBE_TALK_CHANNELS='123456789012345678:lead team:rw,987654321098765432:build noise:ro' \
  vibe-talk:v0
```

Or with a mounted configuration file (`:ro,Z` keeps SELinux hosts happy):

```sh
podman run --rm --name vibe-talk -p 8080:8080 \
  -v ./vibe-talk.toml:/etc/vibe-talk/vibe-talk.toml:ro,Z \
  vibe-talk:v0
```

Behind a tunnel or a reverse proxy, publish to loopback only, so the container has no path in
except the thing you put in front of it:

```sh
podman run --rm --name vibe-talk -p 127.0.0.1:8080:8080 \
  -e VIBE_TALK_DISCORD_BOT_TOKEN='your-bot-token' \
  -e VIBE_TALK_READ_TOKEN='your-read-token' \
  -e VIBE_TALK_WRITE_TOKEN='your-write-token' \
  -e VIBE_TALK_CHANNELS='123456789012345678:lead team:rw' \
  vibe-talk:v0
```

The image runs as a non-root user and contains no configuration; a container started with no
credentials fails immediately rather than coming up with defaults.

**None of the commands above keep anything.** The image declares a volume at `/var/lib/vibe-talk`
and points `VIBE_TALK_STORAGE_PATH` into it, but a container run without a mount writes into its
own writable layer, which `podman rm` — and therefore every rebuild and every redeploy — destroys
without a word. Mount a host directory over it:

```sh
mkdir -p -m 700 ~/.local/share/vibe-talk
podman run --name vibe-talk -p 127.0.0.1:8080:8080 \
  -v ~/.local/share/vibe-talk:/var/lib/vibe-talk:Z \
  -e VIBE_TALK_DISCORD_BOT_TOKEN='your-bot-token' \
  -e VIBE_TALK_READ_TOKEN='your-read-token' \
  -e VIBE_TALK_WRITE_TOKEN='your-write-token' \
  -e VIBE_TALK_CHANNELS='123456789012345678:lead team:rw' \
  vibe-talk:v0
```

`scripts/run.sh` does this for you: it creates `$VIBE_TALK_DATA_DIR` (default
`${XDG_DATA_HOME:-$HOME/.local/share}/vibe-talk`) mode `0700`, mounts it, and reports it under
`--status`. See **Durable state** below for what ends up in there.

## The startup channel probe

At startup, after the configuration is loaded and before the listener is bound, the server reads
**one message from each configured channel** and refuses to start if it cannot.

This exists because of a real failure. The server was configured, it started cleanly, it logged
its channels — and the bot had never been added to the channel. Nothing said so. The mistake
surfaced later as an empty result at first use, which reads exactly like a bug in this code.
That is a silent failure, and this project's rule is that an expected side effect which cannot
happen gets reported explicitly rather than skipped quietly.

**It only reads.** One message, per channel, with `limit=1`. It does **not** post, not even to a
channel configured `rw` — a startup check that wrote to your channel on every restart would be its
own bug. Nothing in `src/probe.rs` can reach the posting call, and a test asserts the in-memory
Discord recorded zero posts after a probe.

**It tells the causes apart**, because they have different fixes. Discord's numeric error codes,
not just the HTTP status, decide which message you get:

| What Discord says | What it means | What to do |
|---|---|---|
| 401 | The **bot token** is wrong, expired, or regenerated | Reset the token in the Developer Portal and update `VIBE_TALK_DISCORD_BOT_TOKEN`. This one is global, so the probe stops after the first channel instead of repeating itself. |
| 403, code `50001` (Missing Access) | The bot **cannot see this channel at all**: either it is not in the server, or it has no `View Channel` there | Re-run the OAuth2 invite for the right server; then, for a private channel, add the bot or its role in **Edit Channel → Permissions**. Discord does not distinguish these two, so the message names both. |
| 403, code `50013` (Missing Permissions) | The bot **can** see the channel but may not read its history | Grant **Read Message History** on that channel. The invite is fine; this is a per-channel override. |
| 404, code `10003` (Unknown Channel) | Wrong snowflake, or a channel this bot was never invited to | Re-copy the id (Developer Mode → right-click → Copy Channel ID); if the id is right, re-check the invite. |
| 429 that did not clear | Rate-limited beyond the client's own retry budget, so readability was **not** established | Wait and restart. The client already waited the time Discord asked for; if it did not clear, something else is very likely sharing this bot token. See "Discord rate limits". |
| no response | The process cannot reach `discord.com` | Check egress: DNS, outbound HTTPS, proxy. |
| success, but every message has **blank content** | Almost certainly **Message Content Intent** is off | Turn it on (Developer Portal → Bot → Privileged Gateway Intents) and restart. |

That last row is a **warning, not a refusal**, and deliberately so: a message can legitimately be
attachment- or embed-only, so refusing to start on it would be a false alarm the operator cannot
clear. It is the one misconfiguration Discord reports as a *success*, so it is called out by name
rather than left to be discovered as "the digest is empty".

**Failure is fatal.** The server names every channel that failed, says why, says what to do, and
exits non-zero. That is the same posture the configuration already takes toward two identical
tokens or a token under 24 characters: for a single-user tool, a start in a state you did not mean
is worse than a refusal.

**Skipping it, for offline development:**

```sh
cargo run -- --config vibe-talk.toml --skip-startup-probe
# or
VIBE_TALK_SKIP_STARTUP_PROBE=1 cargo run -- --config vibe-talk.toml
```

The skip is logged as a warning on stdout *and* printed on stderr, naming which switch caused it.
An invisible skip would be the same silent start the probe was added to remove. (`0`, `false`, and
the empty string do not skip, so a container runtime that renders an unset variable as `""` cannot
turn the check off by accident.)

Note that `--fake-discord` does **not** skip the probe, and the in-memory Discord does not fake a
pass: it knows which channels it actually has, and answers anything else with Discord's own
404 / `Unknown Channel`. If the fake said yes to every channel, the probe would pass against it
unconditionally and the tests would prove nothing about the real client.

## Discord rate limits

Discord answers an over-quota request with **HTTP 429** and says precisely how long to wait: a
JSON body carrying `retry_after` in **fractional seconds** and a `global` flag, a rounded
`Retry-After` header, and the `X-RateLimit-*` family. This client reads that and obeys it. The
whole of it is `src/discord/ratelimit.rs`, and it is shared by the live HTTP client and by the
in-memory fake, so the engine the tests exercise is the engine production takes.

**A 429 is no longer a caller's problem.** It is waited out and the request retried, so a question
asked during a burst is answered a fraction of a second later instead of failing.

**The waiting is bounded, twice**: at most four attempts and at most thirty seconds of total
waiting per request, whichever comes first. A wait longer than the remaining budget is refused
*before* it is taken — parking a caller for a minute and then failing anyway is the worst of both.

**Running out is loud.** The budget is not a way of failing quietly: it produces
`DiscordError::RateLimited`, whose message names the rate limit, the route, how many attempts were
made, how long was actually spent waiting and what Discord still wants. It is never turned into an
empty channel or a generic upstream error.

**Global and per-route are different things and are kept apart.** A per-route 429 shuts one
bucket; a `global` one shuts the whole bot token, and every channel waits behind it. Filing a
global limit under the channel that happened to observe it would let the next channel spend a
request walking into the identical wall a millisecond later.

**A request that is certain to be rejected is not sent.** A 429, or a *successful* response whose
`X-RateLimit-Remaining` is `0`, closes that bucket until it resets; the next request on it waits
rather than being spent. Where Discord's `X-RateLimit-Bucket` says two routes share one bucket,
they share one gate.

**The poll loop's backoff and this are one mechanism at two scales, not two mechanisms.** The
client's wait is the inner, precise one Discord asked for; only a limit it could not clear reaches
`src/live.rs`, and that loop's per-channel doubling then waits *at least* as long as Discord's
outstanding request. The outer wait can be longer than Discord asked for, never shorter.

**All of this is tested against the in-memory fake, and only against the fake.** The fake can
return Discord's 429 with a `Retry-After` and can carry the bucket headers on a success, and the
tests measure the waits on a paused clock. The header names, the body field and the rounding
behaviour come from Discord's published documentation; none of it has met live Discord.

## Slack

The Slack provider reads and posts in Slack conversations — public and private channels, direct
messages, group messages — and in single threads, through a small fixed part of the Slack Web
API. Everything the app does with a Discord channel works with a Slack one: the main timeline,
the thread list, one thread, the flat view, posting and replying, adding a channel by pasting a
link, and the channel browser.

### Setting up a Slack app

1. At <https://api.slack.com/apps>, **Create New App → From scratch**, and pick the workspace.
2. Under **OAuth & Permissions**, add these scopes — as **Bot Token Scopes** for a bot token, or
   as **User Token Scopes** for a user token:

   ```text
   channels:history groups:history im:history mpim:history
   channels:read groups:read im:read mpim:read
   users:read chat:write
   ```

   `*:history` reads messages, `*:read` lets the channel browser and registration see
   conversations, `users:read` turns user ids into names, and `chat:write` posts. Leave out
   `chat:write` for a read-only deployment. Add `channels:write groups:write im:write mpim:write`
   as **User Token Scopes** only to [clear unread in Slack](#clearing-unread-in-slack).
3. **Install to Workspace** and copy the token.

**Bot or user token.** A bot token (`xoxb-…`) posts as the app and reads only conversations the
app has been added to: `/invite @your-app` in each channel, and direct messages only with the
app itself. A user token (`xoxp-…`) reads what you can read and posts **as you**; set
`owner_user_id` to your own id, so your own messages are drawn as yours either way. Your id is
under your profile's **⋮ → Copy member ID**.

### Configuration

```toml
[[providers]]
key = "slack"
kind = "slack"
name = "Slack"                       # the name the app shows
api_base = "https://slack.com/api"   # the default; or a compatible bridge, below
token = "xoxb-…"                     # secret
owner_user_id = "U0123ABCD"          # your own Slack member id
request_timeout_seconds = 20
channel_registration = true          # accept Slack links in "Add channel"
registered_channels_writable = false # whether channels added that way may be posted to
channel_discovery = true             # offer the channel browser
live_poll_seconds = 30               # 0 is off; see "Live push"
upstream_clear = false               # offer "Clear unread in Slack" in Settings; user token only

[[channels]]
id = "C0123ABCD"
label = "team chat"
writable = true
provider = "slack"

[[channels]]
id = "C0123ABCD~1700000000.000100"   # one thread of that conversation
label = "release thread"
writable = false
provider = "slack"
```

**Channel ids.** A whole conversation is its Slack id: `C…` for a channel, `G…` for a private
one, `D…` for a direct message. A channel narrowed to **one thread** is
`{conversation}~{thread ts}`: its main view is that thread, root first, posts go into it, and it
offers no thread navigation of its own. "Add channel" accepts a conversation id, a link copied
from Slack (`https://<workspace>.slack.com/archives/C…`, or a message link `…/p1700000000000100`,
which adds that message's thread), or a web-client link (`https://app.slack.com/client/T…/C…`).
Adding creates nothing in Slack, so removing a channel from the app changes nothing there either.

**Message ids.** Slack names a message by its `ts`, seconds and microseconds. vibe-talk orders,
pages and dates messages by Discord-style snowflakes, so a `ts` is encoded as one —
`((ms − 1420070400000) << 22) | (µs % 1000)` — which orders exactly as the `ts` does, carries its
millisecond, and decodes back exactly. A message from before 2015 cannot be encoded and is not
shown. A thread's id is its root's `ts`.

**Replies.** Slack has no reply pointer; its only reply is a thread reply. Replying to a message in
a whole conversation posts in that message's thread, starting one when it has none. Since any
message can head a thread, a message nobody has answered is given a thread of its own — its `ts`,
with no replies — as Slack itself writes one down at the first reply; that is what lets the app's
grey **Thread** start it, by posting with `thread_ts` set to the message's `ts`. Only user
mentions (`<@U…>`) in a posted message notify anyone: `<!here>`, `<!channel>` and the like are
escaped, as `@everyone` is suppressed on Discord.

**What is shown.** Joins, leaves, topic changes and other system events are left out; ordinary,
bot, file-share, `/me` and thread-broadcast messages are kept. Slack's link, channel and
group-mention markup is rewritten into the markdown the rest of the app reads.

### Limits

* **No native Slack socket.** vibe-talk opens no Events API or Socket Mode connection. By default,
  new messages arrive through the provider's `live_poll_seconds` authoritative history reads. A
  deployment with a separate Slack event stream can use it as a content-free change hint, waking
  that same cursor immediately while the interval remains its recovery path; see [Live
  push](#live-push). Replies inside threads are read when a thread, or a channel narrowed to one,
  is opened.
  For the Meta CLI bridge, make the user-token subscription explicit for the whole conversation.
  An ambient `slack.message tail` depends on the token's existing inbox subscriptions and may
  produce no match; the channel-scoped subscription used by the service is:

  ```sh
  meta slack.conversation tail --channel-id=C0123ABCD --event-types=message.created \
    --catchup=oldest --include-iris-seq-id --quiet --output=json
  ```

  Its stream omits posts by other bots, edits, and deletes, so use it only as the hint command,
  never as the message source. A later hint makes the authoritative forward read collect any
  missed, newer creates — including bot posts — and periodic reconciliation covers a stretch with
  no emitted hint. Neither forward path detects an edit or deletion of an already-cursored
  message; change hints do not imply otherwise.
* **Read state.** What you have read is vibe-talk's own record, as on Discord, and there is no
  "Mark read in Slack" on a message. With `upstream_clear` on, Settings can clear a channel's unread
  state in Slack as vibe-talk receives its messages, but never a thread's; see [Clearing unread in
  Slack](#clearing-unread-in-slack).
* **Bounded reads.** A page Slack (or a bridge) returns may hold fewer messages than were asked
  for; reads follow the cursor and stop at a fixed page budget. A walk never skips a message to
  stay inside that budget: it returns a shorter page, or none, and the next read continues from
  where it stopped. The thread list scans the newest 10 pages of history and says so when it
  stopped there; the flat view includes replies for at most 10 threads per page and the newest
  50 replies of each, and says so when either bound applied.
* **The flat view rereads only threads that moved.** A thread root reports its reply count and
  latest reply, so a flat read reuses the replies the server already holds for a thread whose
  root has not changed (no call), and reads only the newer replies of one that has (one call,
  `conversations.replies` with `oldest=`). A thread whose reply count fell or does not add up is
  read in full, and so is every thread not read in full for fifteen minutes. A full read is
  reused only when it agrees with its root — ending at the latest reply the root names, and
  counting, over the whole thread, as many replies as the root does — so a reply the server had
  not caught up with, one it had not yet dropped, or one posted while the read was under way, is
  read again on the next flat read rather than kept. The saving depends on the server reporting
  `latest_reply` on each root, and on the thread being short enough to count in one read (ten
  pages of replies): a root without `latest_reply`, or a longer thread, is read in full on every
  flat read, as before. An edit to a reply changes nothing on its root, so it can take
  up to those fifteen minutes to show in the flat view, pull-to-refresh included, counted from
  the edit. Opening the thread shows it at once, and the flat view's next read then rereads that
  thread and shows it too, at no extra call for the thread view; a flat read already under way
  when the thread was opened keeps nothing it read of that thread, since it may have read it
  before the edit. Two flat reads that overlap, from two devices or tabs say, are treated the
  same way: one that began before the other finished keeps nothing it read of a thread the
  other read, and drops what the other kept unless the two read that thread alike, so neither
  can take back an edit the other has already shown; at worst the next flat read rereads that
  thread. The root is trusted the other way too: a reply that the thread's replies
  already list, but its root in the channel's history does not yet count, shows in the flat view
  once the root counts it, or at that thread's next full read, within those fifteen minutes,
  where before every flat read showed it at once. Behind a bridge that keeps no cache of its own,
  only Slack itself can lag like that, so it should be brief; opening the thread shows the reply
  at once.
* **Rate limits** are waited out within 30 seconds per request, honouring `Retry-After`; beyond
  that the request fails with a message that names the limit. An author's name never waits: it
  falls back to the user id.

### Clearing unread in Slack

`#258 slack-clear-unread`. vibe-talk mostly reads channels where coding agents post updates, and in
Slack's own client those updates bury the messages from people. With `upstream_clear = true` on the
Slack provider (off by default), Settings offers **Clear unread in Slack** for each of its channels:
the per-channel off, dry run or on switch, its timing, retries, log lines and undo described under
[Local read state is ours; upstream read state is an optional provider
capability](#local-read-state-is-ours-upstream-read-state-is-an-optional-provider-capability). This
is what one push does at Slack:

* **It reads the cursor first.** `conversations.info` reports `last_read`, the account's read cursor
  in the conversation. A message at or before it is reported as already read and nothing is
  written; a newer one is marked with `conversations.mark` at exactly that message's `ts`. A dry run
  only reads.
* **Never backwards.** Slack moves the cursor wherever it is told, backwards too, so that comparison
  is the guard. A conversation whose `last_read` Slack (or a bridge) does not report is refused
  rather than marked blind, and so is one the account is not a member of.
* **Thread replies stay unread in Slack; only the channel is cleared.** A thread you follow keeps a
  read cursor of its own, which `conversations.mark` does not move, and no Web API method open to an
  OAuth token moves it: the one that does, `subscriptions.thread.mark`, is limited to the browser's
  own session tokens. So replies inside threads stay unread in Slack's Threads view, and a channel
  narrowed to one thread (`C…~ts`) is refused outright, because marking its conversation would also
  clear messages vibe-talk never received.
* **Reading and marking are two calls.** Slack has no compare-and-set. If you read further in Slack
  in the second or two between them, the mark moves your cursor back to the message vibe-talk
  pushed, and what you read in that moment shows as unread again. It heals by itself: every message
  vibe-talk receives in the channel is pushed in its turn, so the next push marks those messages
  read again. Re-reading after the mark would not help, because a re-read cannot tell that move from
  the push's own.
* **A refusal stops it for the channel.** `unknown_method` (a bridge without `conversations.mark`),
  `missing_scope`, `not_in_channel`, `not_allowed_token_type`, `channel_not_found`,
  `restricted_action` and the other permission errors stop pushes for that channel until the setting
  is saved again, and Settings says why. Anything else, a rate limit included, is retried with the
  usual growing wait; a rate limit is never waited out inside a push, so it cannot hold up the other
  channels.
* **A user token.** It moves the token's own cursor: with a user token that is yours, and it needs
  `channels:write`, `groups:write`, `im:write` or `mpim:write` by the conversation's kind. With a bot
  token it would move the bot's cursor, which nobody sees.
* **Instants.** The log line's and Settings' `previous` and `target` are RFC 3339 instants with
  microseconds, the same instant as Slack's `ts`: `2026-10-02T23:27:54.800199Z` is
  `1790983674.800199`, which is what a manual undo in Slack's tools needs.

### A compatible bridge

`api_base` may point at anything that implements exactly these Web API methods, as form-encoded
`POST {api_base}/{method}` with `Authorization: Bearer <token>`, answering Slack's JSON:

| Method | Parameters used |
|---|---|
| `auth.test` | — |
| `conversations.info` | `channel`; with `upstream_clear` on, its `channel` must carry `last_read` |
| `conversations.history` | `channel`, `limit`, `cursor`, `oldest`, `latest`, `inclusive` |
| `conversations.replies` | `channel`, `ts`, `limit`, `cursor`, `oldest`, `latest`, `inclusive` |
| `chat.postMessage` | `channel`, `text`, `thread_ts` |
| `users.info` | `user` |
| `users.conversations` | `types`, `limit`, `cursor`, `exclude_archived` |
| `conversations.mark` | `channel`, `ts`; called only with `upstream_clear` on, and answering `{ok:true}` |

A bridge may cap any page below the `limit` asked for, as long as `has_more` and
`response_metadata.next_cursor` say so.

## Checking a deployment

One command, against a running server, local or public:

```sh
scripts/verify-deployment.sh --url http://127.0.0.1:8080 --channel <your-channel-snowflake>
```

It takes the tokens from `$VIBE_TALK_READ_TOKEN` / `$VIBE_TALK_WRITE_TOKEN`, or from
`--read-token` / `--write-token`. Because it takes the base URL as an argument, the **same** checks
run against `http://127.0.0.1:8080` right after `podman run` and against
`https://<your-public-hostname>` once whatever fronts it is up — if the second disagrees with the
first, your ingress changed something.

What it asserts, in this order:

| # | Check |
|---|---|
| 0 | `GET /healthz` answers — nothing below means anything otherwise |
| 1 | unauthenticated `POST /mcp` → **401**, and the body names no tool, channel, service, or protocol revision; `GET /mcp` → **405** |
| 2 | read token `tools/list` succeeds, offers the four read tools, and **does not contain `post_reply`** |
| 3 | read token calling `post_reply` by name → **HTTP 403**, JSON-RPC **-32001** |
| 4 | a channel outside the allowlist → refused with `unknown_channel` |
| 5 | read token `digest_channel` → your **real messages**, non-empty |
| 6 | write token `post_reply` → with "Ask me before the voice agent sends" on (read first from `GET /api/v1/voice-settings`), **NOT SENT**, a proposal, which the script then confirms over `/api/v1/post-proposals/commit`, as the voice page's Send does; with it off, **SENT**, posted at once, and the script says loudly that direct sends are on |
| 7 | the confirmed message is **read back out of Discord** |

It exits non-zero on the **first** failure and names the check that failed; there is no partial
pass and no check that can silently no-op. Failures carry the likely cause — an empty digest, for
instance, points at the Message Content Intent rather than just reporting an empty string.

`--skip-post` drops checks 6 and 7, which are the ones that put a real message in a real channel.
Using it means the half of the system that speaks in your name is untested.

The script is not shipped untried: the vibe-talk CI workflow runs it against the built container
on every push, and then runs it three more times in conditions where it **must** fail (a channel
outside the allowlist, a read-only channel, a wrong token) and fails the build if any of those
passes. A check that cannot go red certifies nothing.

If you would rather do it by hand, the two calls that matter most are:

```sh
# 401, and the body says nothing useful
curl -s -o /dev/null -w '%{http_code}\n' -X POST localhost:8080/mcp \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'

# the read token's tool list — post_reply must NOT be in it
curl -s -X POST localhost:8080/mcp \
  -H "authorization: Bearer $VIBE_TALK_READ_TOKEN" \
  -H 'content-type: application/json' -H 'accept: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

## Surviving a reboot

`scripts/run.sh` launches the container with restart policy `on-failure:5`, which is a policy
about the container *crashing*, not about the machine *rebooting*: `podman-restart.service`
restores only containers whose policy is literally `always`, and on this host it is disabled
anyway. So a hand-started container does not come back after a reboot. On 2026-09-23 that is
exactly what happened — the tunnel unit came back, its origin did not, and the public hostname
served **502 for eighteen hours**: a live tunnel in front of a dead origin.

The fix is a systemd **user** unit that starts the container run.sh already deployed. It builds
nothing and creates nothing, so enabling it cannot change the deployed build.

`~/.config/systemd/user/vibe-talk.service`:

```ini
[Unit]
Description=vibe-talk web app origin
Documentation=https://github.com/rrnewton/agent-utils/blob/main/vibe-talk/README.md
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStartPre=/usr/bin/podman container exists vibe-talk
ExecStart=/usr/bin/podman start --attach vibe-talk
ExecStop=/usr/bin/podman stop --time=10 vibe-talk
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now vibe-talk.service
```

**Linger is the half that is easy to miss.** A user unit does not start at boot unless the user
lingers; without it you have built something that looks fixed and is not. Check, and enable it if
it is off:

```sh
loginctl show-user "$USER" -p Linger      # must print Linger=yes
loginctl enable-linger "$USER"
```

`ExecStartPre` fails loudly when the container has been removed, rather than leaving the unit
"running" in front of nothing. Recreate it with `scripts/run.sh`, then
`systemctl --user restart vibe-talk` so systemd, not your shell, owns the process again — a
redeploy replaces the container out from under the unit.

**Prove it rather than asserting it**, and prove it through the *public* hostname: a local 200
says the process started, which is never what was broken.

```sh
systemctl --user stop  vibe-talk    # then the public URL must go 502
systemctl --user start vibe-talk    # then it must come back 200
```

Rebooting is not the test. The stop/start pair is the control that actually discriminates.

## Asking the server what is wrong

`GET /api/v1/diagnostics`, **read scope**, `Cache-Control: no-store`. It re-runs, on demand, the
checks this server otherwise only runs once at startup, and hands them back **structured** instead
of only printing them to a console you have to still have open from the last restart.

**It always answers HTTP 200.** The report *is* the answer, so a failing check is not an HTTP
error — a 500 here would be indistinguishable from the server being broken in some other way, and
the one thing this route exists to do is tell those apart.

```jsonc
{
  "ok": false,
  "checks": [
    {
      "id": "discord.channel",
      "title": "Discord channel: lead team",  // the label YOU wrote, so you recognise the row
      "subject": "123456789012345678",        // which one, when there is more than one; else null
      "status": "fail",                       // pass | warn | fail
      "summary": "…",                         // the headline, from the shared probe vocabulary
      "detail": "…",       // the evidence: an account name, the vendor's own error body
      "remedy": "…",       // what to do about it; null only when there is nothing to do
      "unconfigured": false // nothing was called, because a setting is absent
    }
  ],
  "passed": 4, "failed": 1, "warned": 1,
  "budget_seconds": 20,
  "took_ms": 812,
  // Only when a bridge has recent_view_shadow on: one row per channel it serves, counts since
  // this process started. See "A bridge's recent read through Flat".
  "recent_view_shadow": [
    { "channel_id": "123456789012345678", "reads": 40, "skipped": 2, "failed": 0,
      "compared": 40, "stale": 1, "raced": 1, "mismatched": 1, "id_mismatches": 1,
      "hash_mismatches": 0 }
  ],
  "upstream": { … }   // how long each provider has taken; see "How long upstream takes" below
}
```

`unconfigured` is kept apart from an ordinary failure on purpose: "the vendor said no" and "you
never gave us a key" are the same red row in a list and are fixed completely differently.

The checks:

| `id` | What it establishes |
|---|---|
| `discord.token` | Discord accepted the bot token, and who it says the bot is |
| `discord.channel` | **one per configured channel** — readable, or why not |
| `elevenlabs.api_key` | the key was accepted, and which account/workspace it reached |
| `elevenlabs.agent` | the configured agent exists and reports a voice |
| `elevenlabs.voice` | a voice resolves — configured explicitly, or borrowed from the agent |
| `storage` | the store can be written to, or the named reason it cannot |

`discord.channel` carries **the same five distinguishable causes the startup probe already tells
apart**, because they have five different fixes: no OAuth2 invite; a per-channel permission
missing; a mistyped snowflake; a bad token; and the Message Content Intent toggle, which produces
no error at all and reads as an empty channel. See **The startup channel probe** above for the
full taxonomy — this route serves that same classification over HTTP rather than only into stdout.

Four properties are the point of it:

* **Every failing check carries a `remedy`** saying what to do next. That is the whole reason this
  exists: it replaces "unavailable" — a word that tells a reader nothing they can act on — with a
  sentence naming the setting, the toggle, or the permission.
* **It never contains a credential.** Every string in the report goes through the same redaction
  the vendor error paths use, against every secret the deployment holds. A vendor is free to quote
  a key back inside its own error text, and this route reports vendor error text.
* **It makes live vendor calls, so it is bounded.** Each check has its own time budget and the
  whole report has an overall deadline (`budget_seconds`). A vendor that does not answer is a
  **failed check with a reason**, never a hung request.
* **Read scope, not write.** It reads configuration state and pokes vendors; it changes nothing.

**Honesty note.** The ElevenLabs and Discord halves of this have been exercised **only against
in-memory fakes, never against live Discord or live ElevenLabs.** The classification is the same
code path the startup probe uses, which is itself in the same position — see **Known gaps**.

### How long upstream takes

The report also carries an `upstream` section: how long each chat provider has taken to answer
since this process started, and how big the messages it sent back were. It is not a check and never
fails. It is there so a slow page can be traced to the provider or ruled out, and so the size of a
message cache can be worked out from real messages rather than guessed (`#208 persistent-cache`).

```jsonc
"upstream": {
  "since": "2026-10-13T07:00:00Z",   // when this process started counting
  "seconds": 5400,
  "providers": [
    {
      "provider": "slack", "name": "Slack",   // the key from the configuration, and its name
      "operations": [                         // one row per operation and outcome
        { "name": "fetch_recent", "outcome": "ok", "count": 41, "rows": 2050,
          "mean_ms": 910.4, "p50_ms": 812.5, "p90_ms": 1937.5, "p99_ms": 3875, "max_ms": 4102,
          "octaves_ms": [[256, 3], [512, 22], [1024, 14], [2048, 2]] }
      ],
      "shim_calls": [                         // one row per Web API method and outcome
        { "name": "conversations.history", "outcome": "rate_limited", "count": 2, "…": "…" }
      ],
      "content_bytes": { "messages": 2050, "total": 553500, "mean": 270, "p50": 140,
                         "p90": 610, "p99": 2900, "max": 11800, "octaves": [[0, 12], [1, 3]] }
    }
  ]
}
```

* **Operations** are the provider-neutral reads and posts the rest of the server asks for
  (`fetch_timeline`, `fetch_recent`, `post_message`, …). The page, the digest and the voice
  catch-up read a channel's newest messages as `fetch_recent_dated`, the same read with the
  provider's `as_of` (see "A bridge's recent read through Flat"); a few smaller reads, such as the
  live poll's first, still count as `fetch_recent`. Operations are timed around each provider's
  own client, so the live poller and this report's own token check count as well as the page and
  the voice agent. Questions a provider answers from memory, such as whether it supports threads,
  are not counted. `rows` is what the operations returned, in total.
* **Shim calls** are the HTTP requests the Slack client sends, by Web API method. One operation can
  be several: a flat read is about eleven, and each author lookup is one more. A request answered
  with a 429 and then sent again counts twice, as the far end's own log counts it (a compatible
  bridge's, for example), so the two can be compared hour by hour. The time here runs from sending
  the request to reading the whole answer, so it includes the network both ways. Other providers
  leave this list empty.
* **Outcomes** are `ok`, `transport`, `rate_limited`, `unauthorized` (401), `forbidden` (403),
  `not_found` (404), `client_error` (another 4xx), `server_error` (5xx), `unexpected_status`,
  `shape` (an answer this server could not read), `refused` (this server said no before asking),
  `cursor_expired`, and `abandoned`: the caller stopped waiting, so the time is how long it waited.
* **Percentiles are read off a histogram** of powers of two from 1 ms to 128 s, each split into
  eight steps, so a percentile is within about 6 % of the true value. `octaves_ms` counts per power
  of two: `[512, 22]` is 22 that took at least 512 ms and under 1024 ms, and `0` means under 1 ms.
* **`content_bytes`** counts the UTF-8 bytes of each message's text, for every message a read
  returned, in powers of two from 1 byte to 1 MiB (`0` is an empty message). A message read twice
  counts twice.
* **Memory only.** A restart starts from zero.

**No channel text.** The section names a provider by its key and display name, which the checks
above already show, and an operation or method by its name in this server's code. Everything else
is a number: no channel, no message id, no author, no text.

**Every hour, one `info` line per provider** gives the same figures for that hour alone, starting an
hour after the server starts. Its target is `vibe_talk::upstream_profile`:

```text
INFO vibe_talk::upstream_profile: upstream timings for the last hour provider="slack" seconds=3600 operations=88 operations_failed=1 operation_timings="fetch_recent ok n=60 p50_ms=812.5 p90_ms=1937.5 max_ms=4102; fetch_recent transport n=1 p50_ms=10000 p90_ms=10000 max_ms=10000; …" shim_calls=402 shim_calls_failed=2 shim_call_timings="conversations.history ok n=300 p50_ms=687.5 p90_ms=1437.5 max_ms=3010; …" content_messages=1900 content_bytes_p50=140 content_bytes_p90=610 content_bytes_max=11800
```

## Summaries

**The problem.** The messages this project exists for are long, and threads pile up faster than
anyone reads them. On a phone, and above all by ear, you need one line per long message and, for
each thread, a short name you can find it by and one sentence on where it stands.

**What you get.** With a summariser configured, a long message (over `summaries.threshold_chars`)
can be collapsed to one plain sentence, and every thread in the thread list gets a **display name**
such as `runner-wedged` — chosen once and then kept, even as the thread drifts, so you always find
it under the name you first learned — and a **sentence** that is refreshed as the thread grows, at
1, 2, 4 and 8 messages and then every 10.

**What it depends on.** A model provider you can reach over HTTPS: the Anthropic API, Google
Vertex, or a gateway that speaks the Vertex path. Nothing else — no SDK, no plugin, no agent
framework. The default model is `claude-haiku-4-5`, a small fast model whose name is valid on both.

### Choosing the summariser

* **`[summaries.llm]` present** — summaries and thread names come from **one plain Messages API
  call apiece**. This is the recommended setup.
* **absent** — summaries come from the ElevenLabs conversational agent in `[elevenlabs]`, and
  threads are not named. Without ElevenLabs credentials either, every summary fails visibly
  (a red row, `503 summarizer_not_configured`) rather than falling back to truncation.

The startup log names the summariser, the dialect and the model, never a credential.

### The Anthropic API

```toml
[summaries.llm]
dialect = "anthropic"
api_key_file = "/run/secrets/anthropic-api-key"   # the key alone, in a file only this server can read
# model = "claude-haiku-4-5"                       # the default
```

Sent as `POST https://api.anthropic.com/v1/messages` with `x-api-key` and
`anthropic-version: 2023-06-01`.

### Google Vertex

```toml
[summaries.llm]
dialect = "vertex"
project = "my-gcp-project"
region = "global"                                  # or a region such as "us-east5"
bearer_token_command = ["gcloud", "auth", "print-access-token"]   # run without a shell
# bearer_token_file = "/run/secrets/vertex-token"  # instead: a token some other process refreshes
```

Sent as `POST https://aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:rawPredict`
(or `https://{region}-aiplatform.googleapis.com/v1/...` for a named region), with the model in the
URL and `"anthropic_version": "vertex-2023-10-16"` in the body. A command's token is reused for five
minutes and dropped early on a 401. **No `x-api-key` is sent on this dialect** unless you set
`api_key_file` here yourself.

### A gateway that speaks the Vertex path

Some organisations put models behind an HTTPS gateway that serves the Vertex path and
authenticates callers by **client certificate**:

```toml
[summaries.llm]
dialect = "vertex"
base_url = "https://models.gateway.example/v1"
project = "my-project"
region = "global"
client_identity_file = "/path/to/identity.pem"     # certificate chain + private key (PKCS#8, RSA or SEC1)
ca_bundle_file = "/path/to/private-root.pem"       # only if the gateway chains to a private root

[summaries.llm.extra_headers]
x-client-name = "vibe-talk"                    # only if the gateway asks callers to identify themselves
```

The identity file is **re-read whenever its modification time changes**, so a short-lived
certificate rotated on disk is picked up without a restart. The CA bundle is added to the bundled
public roots, never instead of them. A gateway's plain-text error, and its error-code header, are
reported in the failure.

### Every setting

| Setting | Default | Notes |
|---|---|---|
| `dialect` | `anthropic` | `anthropic` or `vertex` |
| `base_url` | per dialect | `https://` only; plain `http://` is accepted for a loopback address (a local mock or sidecar). No credentials, query or fragment |
| `model` | `claude-haiku-4-5` | letters, digits, `-`, `.`, `_`, `@`. Never taken from `summaries.model` |
| `project`, `region` | — | Vertex only, and required there |
| `api_key_file` | — | **required for `anthropic`**; on `vertex`, sent only when set |
| `bearer_token_file` | — | Vertex only; re-read on every call |
| `bearer_token_command` | — | Vertex only; an argument list, run without a shell, 20-second limit; not with `bearer_token_file` |
| `client_identity_file` | — | PEM certificate chain and private key; re-read when it changes |
| `ca_bundle_file` | — | PEM roots to trust in addition to the public ones |
| `extra_headers` | none | a table of headers sent on every call; **not for secrets**: the headers this server sets itself (`x-api-key`, `authorization`, `anthropic-version`, `content-type`, …) are refused, and so is any name containing `auth`, `cookie`, `credential`, `key`, `passwd`, `password`, `secret`, `session`, `signature` or `token` |
| `max_tokens` | `120` | output budget per call, `1`–`4096` |
| `timeout_secs` | `30` | per HTTP request, `1`–`600`; also how long, in all, a summary may wait for rate-limit slots |
| `requests_per_minute` | `15` | client-side ceiling across summaries and thread names; at least `1` |
| `burst` | `3` | calls that may go out back to back before the pace applies; at least `1` |

Every path must be absolute. **Secrets are only ever read from files or a command, at call time,
and never logged**; writing `api_key` or `bearer_token` inline is refused at startup by name. Of
this section, only `dialect`, `model` and `max_tokens` are part of the summary cache key: changing
one re-derives every summary and thread sentence (thread names are kept; see below), while
rotating a credential or moving an endpoint does not. Of the other `[summaries]` settings, every
one that changes what a summary says is part of the key too; `threshold_chars` and
`reply_timeout_seconds` are not (see [Summaries are kept](#summaries-are-kept)).

### No tools, by construction

Summarising is "here is some text, say what it is", so the request is one system instruction and
one user turn — and the request type in `src/summarize/messages_api.rs` **has no `tools` or
`tool_choice` field at all**, so nothing can attach one, and a test asserts the serialized body
carries neither. Everything another person wrote travels inside the untrusted-content fence, never
in the instruction. A model offered no tool cannot call one, whatever the channel text asks of it.

### The rate limit

Every call — summaries and thread names alike — goes through one client-side token bucket:
`burst` at once, then `requests_per_minute`. Calls over the limit **queue, first come first
served**. A summary someone is waiting for has `timeout_secs` in all to get a slot, retries
included; when its place in the queue is further away than that, it is turned away at once and
says it was rate-limited. Background thread naming queues separately, behind every waiting summary,
and leaves one token spare for the next one.

A `429` from the provider pauses **every** caller for its `Retry-After` (at most fifteen minutes)
and then resumes at the steady pace. A pause only ever takes allowance away — nothing accrues
while the provider is refusing — so even a provider that answers everything with `429` and
`Retry-After: 0` is called no faster than the configured limit. A `429`, `503` or `529` is tried at
most three times in all.

### Thread names

Names are produced **in the background, never on the read path**: a thread-list read by a
write-scope token queues the threads that are due, and they are named one at a time — read the
thread's root and newest messages, one call, file the result. A read token is shown names that
exist but never causes a call. A name produced after a page was read reaches it on its next
refresh, a forward delta included. A thread whose naming failed is left alone for ten minutes. The
model must answer a name of two to four lowercase hyphenated words (`[a-z0-9-]`, at most 40
characters); any other answer fails that naming by name rather than being cut into shape.

**A name is chosen once and kept.** Both halves live in the summary cache, by different rules:

* The **name** is filed outside every summary policy. Changing the model, a prompt, `max_tokens` or
  any other summary setting keeps it, and ordinary summary traffic cannot evict it — names are
  counted separately against `storage.max_summaries`. A store that cannot be read is never taken
  to mean "no name": nothing is queued for that thread, and a naming re-reads the filed name before
  writing, so a hiccup cannot give a thread a second name.
* The **sentence** is filed under the summary policy like any summary, so a policy change asks for
  a fresh sentence — and keeps the name.

Names are still subject to `storage.summary_retain_days`, counted from the last time the name was
written: on every naming and sentence refresh, and at most daily when a thread list read with the
write token shows the thread active since (the sentence's row is kept alive at the same time). A
thread that stays quiet, or unread, for longer than that gets a new name when it is next listed.
Taking a channel off the list deletes its sentences and keeps its names, so it comes back under
the names you knew. The purge removes names with everything else. With no `storage.path`, names
last for the life of the process.

### What leaves this server

**The text of every message being summarised, and of every thread being named, is sent to the
model provider you configured**, together with its authors' display names. Nothing else is: no
read state, no tokens, no other channel. Choose the provider accordingly, and leave
`[summaries.llm]` out where that is not acceptable.

### Summaries are kept

**The problem.** A summary costs a model call, and the reader sees one row per long message on
every page load. A summary the store already holds should never be asked for again because the page
was reloaded, the server restarted, or a setting changed that changes no word of it.

**What you get.** `#208 persistent-cache`, milestone `summary-keep`:

* **A hit costs no chat call.** The page asks for a summary of a row it has just drawn, so the text
  is still in the server's thirty-second read-ahead memory (the one read-aloud uses); a stored
  summary of exactly that text is answered from there, without reading the channel again. A page
  reload in summary mode with twenty long rows therefore costs no chat calls when the summaries are
  asked for within thirty seconds of the page's own read, and one shared window read otherwise:
  rows that miss together share one fetch rather than one each. That memory is cleared by an
  adapter's pushed edit or delete, by a delta that reports a deletion, and by the purge, so a
  summary is never made from text older than the page was shown and a pushed delete is honoured at
  once. On a provider whose feed carries no deletes, a message deleted upstream in those thirty
  seconds can still have its stored summary shown — the same window read-aloud accepts.
* **Only a change of wording makes a summary unreachable.** The cache key's policy version hashes
  the prompt and whatever the summariser adds to it, the backend, `model`, `target_chars`,
  `context_messages`, `max_per_socket`, `socket_idle_seconds`, and `[summaries.llm]`'s `dialect`,
  `model` and `max_tokens`. `threshold_chars` (which messages are summarised) and
  `reply_timeout_seconds` (how long one is waited for) are not in it, and neither is the length an
  answer is cut at. The version is `v2-…`; the first start after this change re-keys every summary
  filed under the previous `v1-…` version of the same configuration instead of deleting it, and
  logs `re-keyed=N swept=M` at INFO on every start — `re-keyed=0 swept=0` once there is nothing
  left to move.
* **An answer is stored whole and cut when it is shown.** A summary is filed as the model wrote it
  (flattened, up to four times the cut length, as a backstop against a model that ignored the
  instruction) together with the length it was cut at, if it was. It is cut to the length in force
  when it is served: 640 characters for the default `target_chars` of 160. So a change of that
  length needs no new answer — unless a stored one was cut shorter than the new length: then a
  write-token request asks for it again and files the whole answer, and a read-token request is
  shown the shorter text it has, with no model call. Thread sentences are kept the same way.
* **What is spent is counted.** `GET /api/v1/diagnostics` has a `summaries` section of counts since
  the server started: `requests`, `hits`, `hits_without_upstream`, `shared_fetch_joins`,
  `upstream_window_fetches`, `misses`, `generated`, `regenerated_below_cap` and
  `served_below_cap_read_scope`. Numbers only; no key, id or text is logged for any of it.

**What it depends on.** A store (`storage.path`); without one every summary is generated afresh,
as before. Nothing else.

### One config file, two hosts

A configuration file shared between hosts cannot carry one host's credential paths or gateway. Give
that host an **overlay** instead:

```sh
vibe-talk --config /etc/vibe-talk/shared.toml --config-overlay /etc/vibe-talk/this-host.toml
# or
VIBE_TALK_CONFIG_OVERLAY=/etc/vibe-talk/this-host.toml vibe-talk --config /etc/vibe-talk/shared.toml
```

where `this-host.toml` holds just the `[summaries.llm]` section. An overlay's tables **merge** into
the configuration key by key; a key it sets replaces the shared file's, and everything it does not
mention is left alone. An array is replaced whole, so an overlay that writes `[[channels]]`
replaces the channel list.

**`[summaries.llm]` is replaced whole, not merged.** An overlay that names it carries the complete
section and inherits nothing from the shared file's. That is deliberate: a host moving from the
shared file's `dialect = "anthropic"` to its own Vertex gateway must not inherit the shared
`api_key_file`, or the Anthropic key would be sent as `x-api-key` to the gateway. A partial section
in an overlay is checked on its own and refused by name if it cannot stand alone.

`--config-overlay` repeats and applies in order; `VIBE_TALK_CONFIG_OVERLAY` takes `:`-separated
paths applied after the flags. Every file is checked on its own before anything is merged, so a
mistake is reported against the file that has it, at its own line. A named overlay that does not
exist stops the server; each one in use is named in the startup log.

**Honesty note.** All of this is tested against a local mock Messages API that records what it is
sent — the URL, every header, and the body — and answers text, plain-text gateway errors, `429`s
and overloads. The client certificate is tested in a real TLS handshake against a loopback server
that requires one: the configured identity is the one presented, a rotated one is presented after
it changes on disk, and a client without one is refused. It has not yet been run against a live
model provider or a real gateway.

## Configuration

| Setting | File | Environment | Notes |
|---|---|---|---|
| Bind address | `server.bind` | `VIBE_TALK_BIND` | `0.0.0.0:8080` in a container |
| Public URL | `server.public_base_url` | `VIBE_TALK_PUBLIC_BASE_URL` | informational |
| Time zone | `server.timezone` | `VIBE_TALK_TIMEZONE` | IANA name, default `UTC`; an unknown one refuses to start |
| Fetch size | `chat.default_fetch_limit` / `chat.max_fetch_limit` | — | messages a read returns by default and at most, default `50` / `100`; older files set them under `[discord]`, and setting one in both places is refused |
| Count ceiling | `chat.max_count_scan` | — | how many messages a count may walk, default `500`; also accepted as `discord.max_count_scan` |
| Provider request timeout | `discord.request_timeout_seconds` | — | seconds for one ordinary provider HTTP request, default `20`, range `1`–`120`; compatible bridges may need longer |
| Live poll interval | `discord.live_poll_seconds` | `VIBE_TALK_LIVE_POLL_SECONDS` | seconds between inbound reads per channel; **`0` (default) is OFF**, and under `5` is refused |
| Live adapter token | `ingest.token` | `VIBE_TALK_INGEST_TOKEN` | **secret**, optional, ≥ 24 chars and distinct from both API tokens; authenticates normalized event push and content-free change hints. Full events for a polled provider are refused with `409 channel_is_polled`; hints may safely coexist because they wake the poller's own cursor |
| Channel registration | `discord.channel_registration` | — | **off by default**; enable only when `discord.api_base` is a compatible bridge implementing `POST /channels` and `DELETE /channels/{id}` |
| Upstream read marks | `discord.upstream_read_marks` | — | **off by default**; enable only when `discord.api_base` is a compatible bridge implementing `POST /channels/{id}/read` |
| Chat service name | `discord.provider_name` | — | `Discord`; set to the source service's name (for example, `Google Chat`) when using a compatible HTTP bridge |
| Post size | `discord.max_post_chars` / `discord.max_post_bytes` | — | the most one message may hold, in characters and in UTF-8 bytes; a longer one is cut into parts. Discord itself: 2,000 characters, never more. A bridge: what you set, and 2,000 characters when you set neither, so an upgrade never sends a bridge a longer message than it has accepted before. A bridge with `markup = "google-chat"` is also held to Google Chat's 32,000 bytes and refuses a larger setting; set `max_post_bytes` to what the bridge itself accepts, up to that. Under `500` is refused. Also accepted on every `[[providers]]` entry; see "A long message stays one message where it can" under [What it does in v0](#what-it-does-in-v0) |
| Message markup | `discord.markup` | — | how the service writes message text, for the page's renderer: `discord` (default), `google-chat` for a bridge serving Google Chat, where `*a*` is bold and a link may arrive as `<https://…\|label>`, or `slack` for a bridge passing Slack's text through. A Slack provider is always `slack` and refuses the key. See [How message text is drawn](#how-message-text-is-drawn) |
| Thread protocol | `discord.thread_api` | — | `native` (default) for Discord, `bridge` for the normalized endpoints below, or `off` to disable child-thread timelines; an added source that identifies one upstream conversation remains an ordinary channel-picker entry in every mode |
| Bridge recent read | `discord.recent_view` / `discord.recent_view_shadow` | — | `thread_api = "bridge"` only, refused otherwise: which endpoint answers a read of a channel's most recent messages, `messages` (default) or `flat`, and whether `messages` reads are checked against Flat for diagnostics, one at a time once recent reads stop (default `false`; refused with `flat`) — see [A bridge's recent read through Flat](#a-bridges-recent-read-through-flat) |
| Discord bot token | `discord.bot_token` | `VIBE_TALK_DISCORD_BOT_TOKEN` | **secret** |
| Read token | `auth.read_token` | `VIBE_TALK_READ_TOKEN` | **secret**, ≥ 24 chars |
| Write token | `auth.write_token` | `VIBE_TALK_WRITE_TOKEN` | **secret**, ≥ 24 chars, must differ |
| Channels | `[[channels]]` | `VIBE_TALK_CHANNELS` | `id:label:rw` / `id:label:ro`, comma separated; a fourth field, or `provider` in the file, names the channel's provider |
| More providers | `[[providers]]` | — | see [Several chat providers in one server](#several-chat-providers-in-one-server) |
| A provider's token | `providers.<key>.token` | `VIBE_TALK_PROVIDER_<KEY>_TOKEN` | **secret**; the key upper-cased with `-` as `_`, so `google-chat` reads `VIBE_TALK_PROVIDER_GOOGLE_CHAT_TOKEN` |
| ElevenLabs agent id | `elevenlabs.agent_id` | `VIBE_TALK_ELEVENLABS_AGENT_ID` | public |
| Read-aloud backend | `read_aloud.backend` | `VIBE_TALK_READ_ALOUD_BACKEND` | `elevenlabs` (default), `browser` for the device speech engine, or `conversation` to use the configured `vibe-talk-v1` agent |
| Conversation backend | `conversation.backend` | — | `elevenlabs` (default) or `websocket` for a deployment-managed `vibe-talk-v1` endpoint |
| Conversation WebSocket | `conversation.websocket_url` | — | required for the `websocket` backend; keep private endpoints in deployment configuration. A path such as `/ws?mode=…` means the page's own host, so one configuration serves every host that puts a bridge behind its front door at that path |
| Conversation label | `conversation.label` | — | provider name shown in the UI and its connection errors |
| Prepare bodies for speech | `speakable.enabled` | `VIBE_TALK_SPEAKABLE` | **on by default**; off relays exactly what was typed. Does not affect the time-zone conversion. A value that is neither `true` nor `false` refuses to start |
| ElevenLabs API key | `elevenlabs.api_key` | `VIBE_TALK_ELEVENLABS_API_KEY` | **secret**, needed to mint signed URLs and for the ElevenLabs read-aloud backend |
| Your own Discord user id | `discord.owner_user_id` | `VIBE_TALK_DISCORD_OWNER_USER_ID` | public; **cannot be derived** — see below. Without it, messages you type into Discord yourself are drawn as a third party |
| ElevenLabs voice id | `elevenlabs.voice_id` | `VIBE_TALK_ELEVENLABS_VOICE_ID` | public; **optional** — read-aloud borrows the configured agent's own voice when this is unset |
| ElevenLabs API base | `elevenlabs.api_base` | `VIBE_TALK_ELEVENLABS_API_BASE` | `https://api.elevenlabs.io/v1` |
| Storage path | `storage.path` | `VIBE_TALK_STORAGE_PATH` | absolute; unset means **no durable state** |
| Conversations kept | `storage.max_conversations` | — | default `50`, oldest dropped first |
| Turns per conversation | `storage.max_turns_per_conversation` | — | default `1000`, then `413` |
| Cached summaries kept | `storage.max_summaries` | — | default `2000`, least recently written dropped first |
| "Dealt with" marks kept | `storage.max_dismissals` | — | default `10000`, oldest dropped first; **not** bounded by age, see below |
| Retention age | `storage.retain_days` | — | default `30`; `0` means no age limit; conversations, since their last turn |
| Summary retention age | `storage.summary_retain_days` | — | default: `retain_days`; `0` means no age limit; cached summaries, thread sentences and thread names, since their last write-token use. Keep it within how long the chat provider keeps messages |
| Summary threshold | `summaries.threshold_chars` | — | below this nothing is summarised, default `400` |
| Summary width | `summaries.target_chars` | — | default `160` |
| Summary context | `summaries.context_messages` | — | preceding messages shown as context, default `3` |
| Summary model | `summaries.model` | `VIBE_TALK_SUMMARY_MODEL` | nothing reads it (`[summaries.llm]` names its own `model`); it only moves the cache key |
| Summariser | `[summaries.llm]` | — | present: one Messages API call per summary and thread name, see [Summaries](#summaries); absent: the ElevenLabs agent in `[elevenlabs]`. `summaries.backend` / `VIBE_TALK_SUMMARY_BACKEND` were removed with the extractive summariser: `extractive` is now refused at startup by name, `elevenlabs_agent` starts with a loud warning that it selects nothing, and anything else is refused as unknown. Delete the line |
| Messages API summariser | `summaries.llm.*` | — | dialect, endpoint, model, credentials **from files only**, timeout and rate limit; every key is in [Summaries](#summaries) |
| Agent conversation budget | `summaries.max_per_socket` | — | summaries one conversation serves before it is recycled, default `8`, minimum 1 |
| Agent conversation idle close | `summaries.socket_idle_seconds` | — | default `30` |
| Agent turn deadline | `summaries.reply_timeout_seconds` | — | one turn, and one opening handshake, default `30`, minimum 1 |
| Resuming | `replay.enabled` | `VIBE_TALK_REPLAY_ENABLED` | **off by default**; on, every new call re-sends earlier conversation content to the vendor. Only the ElevenLabs backend can carry it: with `conversation.backend = "websocket"` the page is told replay is off, and the server warns at startup |
| Replay budget | `replay.max_chars` / `replay.max_turns` | `VIBE_TALK_REPLAY_MAX_CHARS` / `_TURNS` | default `6000` / `40`; oldest dropped first; `0` is refused, not "unlimited" |
| Replay transport | `replay.transport` | `VIBE_TALK_REPLAY_TRANSPORT` | `contextual_update` (default) or `client_data` |
| Config file path | — | `VIBE_TALK_CONFIG` | or `--config` |
| Config overlays | — | `VIBE_TALK_CONFIG_OVERLAY` | or `--config-overlay`, repeatable; TOML laid over the config file table by table, see [One config file, two hosts](#one-config-file-two-hosts) |
| Message cache | `[message_cache]` | — | **read and checked, but no cache is opened** by this build, and startup says so (`#208 persistent-cache`). Keys: `enabled` (default `false`), `path` (absolute; default beside `[storage] path`, never equal to it), `max_days` (default `90`, 1 to 365), `max_megabytes` (default `256`, 16 to 4096, counting the `-wal` and `-shm` files), `backfill` (`adaptive`, the default, or `off`), `timeline_fallback_ms` (default `10000`, 1000 to 60000), and `[[message_cache.channel]]` entries of `id` and `max_days` for a channel kept for a different time. `0` is refused, not read as unlimited; an unknown key is refused; `enabled = true` needs `[storage] path`, where the cache's off switch is kept. An absent table is not the same as `enabled = false` |

Environment wins over file. An empty environment variable is treated as unset, so a runtime that
renders unset variables as `""` cannot blank out a configured value. An unknown key in the file is
an error, not a shrug — a typo'd section name should not silently disable a setting.

## Several chat providers in one server

One running server can read Discord, a compatible bridge for another service such as Google
Chat, and Slack at the same time. You serve one copy of the app, sign in once, and pick any
channel from any of them in the same channel picker.

Each provider is a `[[providers]]` entry with a short `key`. Each channel names the key of the
provider it is read through. The legacy `[discord]` section is still accepted: when present, it
becomes the provider keyed `discord`, and channels that name no provider belong to it.

```toml
[chat]
default_fetch_limit = 50

# A Discord-compatible bridge, for example one that serves Google Chat.
[[providers]]
key = "gchat"
kind = "discord"                         # Discord's HTTP API, or a bridge speaking it
name = "Google Chat"
markup = "google-chat"                   # *a* is bold there; see "How message text is drawn"
api_base = "http://127.0.0.1:18765/api/v10"
token = "REPLACE-ME"                     # or VIBE_TALK_PROVIDER_GCHAT_TOKEN
channel_registration = true
upstream_read_marks = true
thread_api = "off"

[[providers]]
key = "slack"
kind = "slack"
token = "xoxb-REPLACE-ME"                # or VIBE_TALK_PROVIDER_SLACK_TOKEN
owner_user_id = "U0123ABCDEF"
live_poll_seconds = 30

[[channels]]
id = "C0123ABCDEF"
label = "team"
writable = true
provider = "slack"
```

Keys common to both kinds: `key` (1–32 lowercase letters, digits, or hyphens), `kind` (`discord`
or `slack`), `name` (display name; defaults to `Discord` or `Slack`), `api_base`, `token`,
`owner_user_id`, `request_timeout_seconds` (default `20`, range `1`–`120`), `live_poll_seconds`
(default `0`, off; at least `5`), `channel_registration`, and `max_post_chars` / `max_post_bytes`
(the most one message may hold; see the configuration table). Discord-protocol providers also take
`thread_api`, `upstream_read_marks` and `markup`, and with `thread_api = "bridge"` also `recent_view`
and `recent_view_shadow`, with the same meaning as in `[discord]`. Slack providers
also take `registered_channels_writable`, `channel_discovery` and `upstream_clear`; see [Slack](#slack). A key that
does not apply to the provider's kind is refused by name.

A Slack provider measures a message as Slack is sent it, after `&`, `<` and `>` become `&amp;`,
`&lt;` and `&gt;`. Pointed at Slack itself it takes 39,000 characters unless configured, a margin
under the 40,000 past which Slack truncates, and refuses a `max_post_chars` above 40,000. Pointed
at a bridge it takes what `max_post_chars` and `max_post_bytes` say, and 2,000 characters when
neither is set.

How a deployment with several providers behaves:

* **Every channel belongs to exactly one provider.** Reads, posts, threads, read marks, and the
  startup probe for that channel go to that provider and no other. A channel with no provider,
  in a deployment that has several and no `[discord]` section, is refused at startup.
* **The page uses each channel's own provider's capabilities.** `GET /api/v1/client-config`
  lists every provider (`providers`) and each channel's `provider` key. A channel read through a
  provider without threads is shown as a flat history even when another provider has threads.
  The top-level capability flags mean "at least one provider can".
* **Adding a channel from the app** asks each provider whether it recognises the pasted link. A
  provider that is certain wins (Slack recognises its own links and conversation ids). Otherwise,
  the only registration-managing bridge that might accept it is used. Otherwise the app asks you to
  put the provider's key and a colon in front of the link, for example `gchat:https://…`. The
  provider that registered a channel is recorded with it, so it stays with that provider across a
  restart.
* **The channel browser** lists each discovery-capable provider in turn.
* **Live updates are per provider.** Each provider with `live_poll_seconds` set is polled on its
  own interval. An adapter may push events for the others through `ingest.token`, or send a
  content-free hint that wakes a polled provider's existing cursor. Pushed events for a polled
  provider's channels are refused, so no change is delivered twice.
* **Diagnostics check each provider's credential** and name the provider in every channel check.

## Durable state

Almost nothing in this server is remembered. Nothing about a channel is written to disk, and
the page and the tools read channels from the chat provider. Two short-lived copies are the
exceptions. Both are in MEMORY only, they are kept apart, and both are gone when the process
restarts. Messages fetched for the page in the last 30 seconds are kept so that preparing them
for read-aloud does not fetch them again (`#191 read-aloud-first-lag`, `src/read_ahead.rs`).
Nothing else reads them, a pushed edit or deletion drops the message, and the storage purge clears
them. Each channel's newest window is kept so the voice agent's catch-up can answer while the
provider is slow (`#210 voice-catchup-speed`, `src/recent.rs`): the last read of each listed
channel plus the changes pushed to it since. Its last 200 pushed changes are kept beside it, so a
read that lands after one of them does not undo it. The storage purge clears the windows and those
changes too, and a read that was already running when it came keeps nothing when it lands, so the
next catch-up reads every channel afresh. These two are not the only channel text held in memory,
and the purge does not clear the others; "How an operator purges it" lists them. There is exactly
one store, `src/store/`, and it holds these:

* the `/voice` **conversation transcript** — what you said and what the agent said back;
* **read marks** — how far you have been shown each channel;
* **"dealt with" marks** — which individual messages you have finished with, which is the
  overlay `#50 todo-view` is built on. Two snowflakes and an instant per row, and **no message
  text at all**; and
* **cached summaries** — one short line per long message, filed under the policy that produced
  it. This is the one entry NOT authored by this server: a summary is a model's paraphrase of
  somebody else's message, written from that message and from the messages around it, so it is a
  second at-rest copy of third-party text and is treated as one everywhere below; and
* **channel aliases** — what THIS app calls a channel, one row per configured channel at most.
  Ours and local in the same sense the read marks are: never sent to Discord, renaming nothing
  there. See "What to call a channel". `#39 channel-alias`; and
* **noise rules and "not noise" marks** — which placeholder messages count as read
  automatically, one list for the whole deployment, and the messages you rescued from it. Two ids
  and an instant per mark, bounded by the same count as the "dealt with" marks. See "Messages read
  automatically". `#196 auto-read-noise`; and
* **hidden configured channels** — which channels from the configuration file you took off your
  list. One id and an instant per row, one row per configured channel at most. Like the channels
  added in the app, these are the shape of the list rather than a record of anything read, so the
  purge leaves both alone. See "Taking a channel off the list". `#199 removable-config-channels`;
  and
* **the voice send setting** — whether the voice agent asks before it sends, as the owner last
  chose it: one row, one boolean and an instant. No row means the configured default applies, and
  the purge deletes the row, restoring that default — which asks first unless the operator set
  `ask_before_send = false`, in which case a purge turns direct sends back on. See "Posting asks
  first, unless you turn that off". `#264 voice-send-approval`; and
* **the owner's instructions for the voice agent** — the additional instructions and his edited
  copy of the system prompt, one row holding two texts he wrote, with the prompt version the copy
  was made from. No row means the shipped prompt; the purge deletes the row. See "Instructions for
  the voice agent". `#272 voice-agent-instructions`.

The first three are your own record. The cached summaries are a derived cache, bounded by the
same retention as the rest, erased by the same purge, and never filled by a read-scope token.

**Every table is bounded on write, and one of them is bounded differently on purpose.** The
"dealt with" marks have a count ceiling and **no age limit**, unlike everything else here. An age
limit would put a message you cleared last month back into your to-do list because time
passed — a lie you cannot diagnose, since nothing on the row would say why it came back — whereas
an expired summary is merely regenerated. The count bound is enough on its own here for a reason
it is not enough for a summary: this is the one table that holds nobody's words.

### Thread backend contract

Thread discovery, membership checks, message lookup, and posting go through `ChatClient`. The
browser uses `threading_supported` and `chat_provider_name` from `/api/v1/client-config`; it does
not infer capabilities or branding from a service name. Existing `[discord]` configuration keys
remain compatible with earlier deployments.

With `thread_api = "bridge"`, a compatible HTTP bridge implements these paths relative to
`discord.api_base`:

* `GET /channels/{id}/timeline?view=main|threads|flat|thread&thread_id=…&before=…&after=…&limit=…`
* `GET /channels/{id}/threads/{thread_id}/messages/{message_id}`
* `POST /channels/{id}/threads/{thread_id}/messages`

Thread identifiers and pagination cursors are opaque. Each path segment is percent encoded;
neither callers nor bridges should interpret a thread as a quoted-message reference. Thread reads
and writes must verify membership in the registered parent channel, including any narrower scope
that registration imposes. Posts use the existing message body and nonce contract. Lookup and
post responses use the existing message wire format.

A thread post's response is vibe-talk's confirmation that the message was delivered, so it must be
the posted message in the parent channel. Its `thread` should name the thread posted to, but it may
be absent; vibe-talk then takes the message to be a reply in that thread, since the route posts
there or fails. A bridge can legitimately leave it out: for example, when its live event stream
stored the message before the provider confirmed the send, and the bridge answers the post (or a
retry under the same nonce) with that stored copy. A response in another channel, or naming another
thread, is refused and the post reported as unconfirmed: the bridge answered, but not for the post
vibe-talk asked for (`send-confirm-thread`).

A message wire object may carry Discord's `reactions` array, each entry
`{"emoji": {"name": "👀", "id": null}, "count": 1}` — a custom emoji has an `id`, and its `name` is
shown as text. vibe-talk reads only the name, id, and count. An absent array means the bridge cannot
say; send `[]` for a message it knows has none. A message whose reactions changed is a changed
message, in forward reads too (`#219 emoji-reactions`).

Timeline responses follow `src/threads.rs`: messages and thread summaries are oldest first,
`has_more` pairs with `next_before`, and `has_threads` controls navigation. Message wire objects
carry an optional `thread` object with `id`, `root_message_id`, `is_root`, `reply_count`, and
`reply_count_exact`. Summary roots use the same message wire format. Discovery must finish before
claiming global chronology or exact counts; a cap or provider failure must report an error instead
of returning a plausible but incomplete flattened page. A notice can explain registration scope.
A registration narrowed to one conversation names that conversation as `thread` in main, flat,
and threads responses. Such a channel has no child threads, so vibe-talk reports no thread
navigation for it and never lists the conversation as a child of itself. Its Main is the whole
conversation: the page draws every message it is served there, the root and its replies in time
order, keeps the replies through a walk back and a reload from the device, and offers no replies
button on the root, which would only open what Main already shows. A channel with child threads
keeps a Main of roots and unthreaded messages.

**Forward reads** (`#203 incremental-refresh`). A compatible bridge may also answer
`GET /channels/{id}/timeline?view=…&thread_id=…&limit=…&after=…`. `after` is the opaque
`next_after` of an earlier response for the same channel, view, and thread; `after` and `before`
are mutually exclusive (`400`). A bridge that supports forward reads includes `next_after` (a
URL-safe string of at most 2048 characters) in every newest-page response for which it will accept
one; vibe-talk learns the capability from that field alone and never probes for it. A bridge that
omits it is read with full newest pages exactly as before.

A response to `after` carries only the entries new or changed in that view since the cursor,
oldest first, at most `limit`, and an object
`"delta": {"more": bool, "complete": bool, "deleted": [message ids], "removed_threads": [thread ids]}`.
`has_more` is `false` and `next_before` is `null`; `next_after` is required and continues the read.
`more` asks the caller to continue at once. `complete: true` promises that edits and deletions
since the cursor are included (`deleted` is channel-wide, at most 500 ids); a bridge that only sees
creations answers `complete: false` with empty lists. In the main view, the root of a thread whose
replies changed is included, because its `reply_count` changed. `thread`, `has_threads`, and
`notice` carry current values. Messages and summaries use the existing wire formats and the same
membership checks as every other timeline read. A response to `after` without `delta` is treated as
the newest page it is.

A cursor the bridge can no longer answer for — issued by an earlier process, older than the changes
it still remembers, or failing verification — gets `410` with
`{"error":"cursor_expired","message":"…"}`; vibe-talk then reads the newest page again. A malformed
cursor, or one issued for another channel, view, or thread, gets `400`. A bridge rolled back to a
version without forward reads may answer an `after` it once issued with `400`, `405`, or `501`;
vibe-talk treats that as an expired cursor too, and the newest read that follows carries no
`next_after`.

A bridge may also include `as_of`, an RFC 3339 instant up to which it knows the response is
complete, when it is serving data it could not bring up to date just now; vibe-talk passes it to
the page, which then says the service is slow instead of claiming the list is current. Absent
means "now".

Every backend can be read forward through vibe-talk's own route, bridge or not. For one that issues
no `next_after` of its own, vibe-talk issues a generic cursor — the newest provider timestamp
served, a two-minute overlap below it, and the entries already delivered inside that overlap — and
answers `after` by reading newest pages and keeping what is newer: correct, with `complete: false`,
and no cheaper than a newest read. The direct backends answer the views one request can serve
themselves: native Discord a thread and a text channel's Main with `after=<snowflake>`, Slack a
conversation's Main and one thread with `oldest=<ts>`. Both read by creation, so they too answer
`complete: false`, and a reply does not bring its root's new count into Main until the next full
read; their thread lists, All views, and a forum's or a narrowed channel's Main use the generic
cursor. A cursor a direct backend did not issue for that view — one a bridge issued before the
channel moved to the native API, say — is answered `410 cursor_expired`, so the page reads the
newest page rather than failing every refresh with it. The page keeps its full poll on all such
channels, so edits and deletions arrive with the next poll, as before — except an edit to a reply
in Slack's All view, which waits for that thread's next full read (see "The flat view rereads
only threads that moved" in Slack's Limits); forward reads there serve live arrivals and a
return to the page.

The native backend includes accessible active and archived threads. It bounds discovery at 256
threads and 100 pages per archived collection, retains pagination inventories for five minutes,
and bounds a timeline request at 90 seconds. Exceeding those limits returns an explicit error.
Native reply counts are marked approximate because older provider counters may be capped. An
expired cursor reloads the newest history with a fresh continuation. Forum and media channel Main
views show thread roots; open a thread to post in these thread-only containers.

### A bridge's recent read through Flat

**The problem.** Every read of a channel's most recent messages — the digest, the to-do list, a
summary's context, read-aloud, and above all the voice catch-up `recent_activity` — goes to
`GET /channels/{id}/messages`. A bridge that answers that by running a provider command takes
about two seconds per channel, one channel at a time. The same bridge answers the newest page of
its Flat view (`/timeline?view=flat`) from a copy of the channel it keeps current, in
milliseconds, and both list every message, replies included, under the same ids.

**What you get.** `recent_view = "flat"` (in `[discord]`, or on a `[[providers]]` entry of kind
`discord`, only with `thread_api = "bridge"`) answers those reads from the newest Flat page of the
same size instead, shaped exactly like a `/messages` window: oldest first, each message once, the
newest `limit` of them. When the bridge says its copy is complete only up to an earlier instant
(`as_of`), vibe-talk says so everywhere that read goes: `GET /api/v1/channels/{id}/messages`
carries the same optional `as_of`; `digest_channel` and `recent_activity` say `AS OF <how long
ago> (exact <instant>)` and that newer messages may be missing; and the window held for catch-ups
counts its age from `as_of`, so it never displaces a window whose picture is newer. The default,
`messages`, reads `/messages` exactly as before. Neither setting touches the timeline route the
page reads.

**Check it first.** `recent_view_shadow = true` (with `recent_view = "messages"`) keeps answering
from `/messages` and checks those reads against later reads of the newest Flat page of the same
size, when and how often as described below. Over the stretch both reads cover — from the newer of
their oldest messages, or the channel's start for a read that came back short, up to the newest
message `/messages` returned — it counts, per channel, messages only one of the two listed
(`id_mismatches`) and messages both listed with different text (`hash_mismatches`, by the same
content hash the summary cache uses). A Flat message newer than everything `/messages` returned
arrived after that read and is not counted, and nothing is compared beside an empty `/messages`
read. An edit or a delete that lands between the two reads is counted like any other mismatch, so
two more counts help explain one: `stale`, a Flat page the bridge labelled with `as_of`, and
`raced`, a comparison across which the live feed reported an edit or a delete in that channel
(always zero without a live feed). The counts are in memory, reset on restart, and appear in
`GET /api/v1/diagnostics` as `recent_view_shadow` — see
[Asking the server what is wrong](#asking-the-server-what-is-wrong). One `info` log line per
comparison that found a difference carries the counts; a `debug` line names the message ids, never
their text. Turn `recent_view = "flat"` on once a day of shadowing shows none, or only ones you can
explain; the setting refuses `recent_view_shadow` beside it.

**What the shadow costs the reads it checks.** A Flat read is not free for a bridge, and it may not
wait its turn there. The bridge this was built against runs every provider command through one slot
and serves a Flat read as one a reader is waiting for, ahead of `/messages`. When its copy of the
channel is a few minutes old it first brings the copy forward, one provider command of about two
seconds; when it holds no copy — the first read of a channel after a quiet spell or a restart of the
bridge — it scans the whole channel, for up to 50 seconds. Either way it stops the `/messages` read
that holds the slot, runs it again afterwards, and goes ahead of every one waiting. So the checks
wait: they run one at a time, from one queue across every channel, each only once no recent read
has run for three seconds. A channel has at most one read waiting — a newer read of it takes its
place — and a read that has waited more than two minutes is dropped, since a Flat read that late
would count every edit and delete in between; both count as `skipped`, as does a read the storage
purge drops, since a waiting read holds the text it read. That makes a collision unlikely, not
impossible. A recent read that starts while a check is making the bridge bring its copy forward
still waits for it, so while the shadow is on, expect a catch-up or a `digest_channel` soon after
a quiet spell or a bridge restart to be slower, by as much as that scan. Once checked, a channel's
copy is kept current by the bridge in the background through the same slot, at the priority of
`/messages` itself: nothing is stopped for that work, but a read can queue behind it.

What it does not do: a Flat page the bridge cannot build (a channel past its copy's size limits)
fails the read rather than falling back to `/messages`, and a bridge copy that is cold is built on
the first read, which can take far longer than `/messages`. Shadowing first shows both. With
`recent_view = "flat"`, every read of a channel's newest messages goes to Flat, not only the ones
above: the channel probe at startup and in `GET /api/v1/diagnostics`, the first read of a live poll,
the check of a channel's newest message when upstream unread clearing starts or is run now, and
`find_message`. Those carry no `as_of`; neither do the to-do list or the JSON of `/digest` and
`/activity`. A cold copy can take a diagnostics probe past its 20-second budget.

### Local read state is ours; upstream read state is an optional provider capability

Discord does not share read state with bots. There is no ack route a bot may call, no read-state
field on the channel object a bot can see, and no `read_state` in the gateway `READY` payload for
a bot user (this is what issue #61 `unread-status` established). So the read marks here are
vibe-talk's own record, and the rule is stated once rather than left to be inferred:

* **No sync-in.** Marking a channel read in the Discord app changes nothing here.
* **No sync-back.** Marking a channel read here acks nothing and posts nothing; the Discord app's
  unread badge does not move.

Anything that shows a read mark to a person has to say that, because "read" already means
something else to a Discord user.

A compatible provider bridge may also implement `POST /channels` with
`{"source":"...","label":"..."}`. Its JSON response has required path-safe string `id` and
boolean `created` fields plus an optional boolean `writable` field, for example
`{"id":"path-safe-stable-id","created":true,"writable":true}`. `created` is `false` when the
bridge reused an existing registration. `writable` defaults to `false` when omitted for compatibility
with older bridges; a present non-boolean value rejects the response and, when `created` is true,
triggers a compensating delete. It is the bridge's policy: vibe-talk persists it and never accepts a
managed-mode writable choice from the UI. Managed message posts carry a path-safe `nonce` generated
once per logical post and kept unchanged across internal HTTP retries; bridges must pass that value
through as their provider request id. When the caller of the reply route sends an `idempotency_key`,
each part's nonce is derived from that key and the part's own text instead, so the caller's retry of
the same text reuses the same request id (`#195 send-resilience`); a `207` says whether its unsent
remainder keeps that property (`resumable`). Direct Discord posts do not carry this bridge-only field. The bridge must
implement idempotent `DELETE /channels/{id}`: deleting an already-absent registration still succeeds
with `204 No Content`. Opt in with `discord.channel_registration = true`. Settings then accepts a
channel link or provider reference, lets the bridge resolve it to the stable id used by vibe-talk,
checks that id is readable, and hides the writable checkbox. Only rows originally registered through
this managed flow are unregistered upstream on removal; direct and pre-migration rows remain local-only.
Direct Discord deployments keep the existing id, label, default-off explicit writable switch, and must
leave this setting off.

A registration-capable bridge may also list the channels its account can see, which lets
Settings offer **Browse channels** (`#19 channel-browser`): a searchable list with one-tap Add and
the channels already tracked marked as such. The bridge implements `GET /channel-directory` with
optional `query` (trimmed, at most 100 characters; blank lists everything), `limit` (1 to 50, default
25) and `cursor` (the opaque `next_cursor` of the previous page of the same query). It answers
`{"entries":[{"source":"...","name":"...","registered_channel_id":"id-or-null"}],"next_cursor":"...-or-null","truncated":false}`,
in its own order and never more entries than `limit`. `source` is handed back unchanged as the
`source` of `POST /channels`; `registered_channel_id` names the bridge's existing registration, if
any; `truncated` says the bridge stopped listing before the end of what its account can see. A
malformed answer is refused rather than repaired. `401`/`403` reach the page as a refusal, and
`404`/`405`/`501` as "this bridge has no directory". vibe-talk probes nothing at startup: it offers
the list whenever `discord.channel_registration` is on, and a bridge without the route simply
answers the first read with one of those statuses. Every string the bridge returns is drawn as text.

A compatible provider bridge may separately implement `POST /channels/{id}/read` and opt in with
`discord.upstream_read_marks = true`. The web app then offers **Mark read in** *the service* under each
message's **⋯** menu, labelled with the scope it moves (the whole space). That action advances the provider's monotone cursor through the selected message; it does
not write vibe-talk's local read mark and does not archive the message. Thread-level behavior is
defined by the provider. The bridge may return JSON or an empty successful response such as `204
No Content`. Direct Discord deployments must leave the setting off.

**Clearing unread automatically, per channel** (`#232 upstream-mark-read`). vibe-talk reads only
some of the owner's channels, and those are mostly coding agents posting updates; in the service's
own client those updates bury the messages from people. With the same bridge opt-in, Settings offers
**Clear unread in** *the service* for each channel whose provider can do it: off (the default), a
dry run, or on. On, the SERVER moves the service's own read marker for that channel forward to the
newest message it has seen there (live, at startup, and when the owner presses **Mark read now**), so
that client's unread list keeps the people and vibe-talk keeps the agents.
vibe-talk's own read marks and Done state are not touched, and nothing is ever read back.

**Thread replies stay unread in Google Chat; only the main conversation is cleared.** Settings says
this in those words beside the switch. Google Chat keeps a separate read state for each thread, and
no supported interface available to the bridge can write it: the public REST and gRPC APIs, and the
command-line client the bridge drives, move only the space's own marker (checked on 2026-10-12).
Moving that marker leaves the replies inside threads unread, and Google Chat's Home feed is mostly
followed threads, so much of what shows as unread there stays unread. Google's Developer Preview MCP
tools for Chat include one that marks a thread read; a bridge with access to it could clear thread
replies too. That is a possible future path, not something any bridge here does today.

* **The setting is the server's.** It is kept in the store (one row per channel set, `off` when there
  is none), because the server acts with no page open. Write scope to change it, read scope to see
  it, and no MCP tool. A server without a store cannot offer it.
* **Ingest never waits for it.** Every created message, polled or pushed by an adapter, is noted in
  memory as its channel's newest. One push per burst: after 5 seconds with nothing newer, or after
  30 seconds of steady traffic, and never sooner than 30 seconds after that channel's last push.
  Pushes run one at a time, in the background.
* **Never backwards.** A message at or before the newest one already covered is never pushed, and
  that floor survives a restart and the setting being turned off. At startup each enabled channel is
  checked against its newest message, so one that arrived while the server was down is still cleared.
* **Failures retry** after 30 seconds, doubling to 15 minutes, plus up to a tenth more per channel,
  and never sooner than a `429`'s `retry_after`. A newer message replaces one waiting to be retried.
  The status line in Settings shows the last push, a failure and its retry time, or why the channel
  cannot be cleared.
* **Mark read now** (`#253 upstream-clear-polish`). While a channel is on, Settings offers a one-shot
  **Mark read now** button, **Try it now** in a dry run. It pushes the channel's newest message at
  once, through the same loop, skipping the quiet period and the spacing between pushes but never a
  retry wait after a failure. The button is disabled until the server answers, and the status line
  shows progress and then what that press came to, said from the push that answered it and never
  from a later one: marked, already read there, what a dry run would mark, failed (and when it is
  tried again), nothing newer to mark, waiting to retry after an earlier failure, the setting changed
  while it was on its way, or still on its way. The server holds the request at most 45 seconds, so
  a common 60-second proxy timeout never cuts it off; still on its way by then, Settings asks again
  every 4 seconds for up to 30 seconds, says what it came to as soon as that is recorded, and
  otherwise says it has not been answered yet. A mode chosen meanwhile and not saved stays chosen,
  with Save offered: no read puts the saved mode back over it. It changes no setting, and a channel
  that is off refuses it, so a page still showing On after another device turned it off cannot turn
  it back on. Turning a channel on pushes nothing by itself: the next message that arrives is pushed,
  and with it everything before it.
* **Every attempt is logged on one line** beginning `upstream-clear:`, naming the mode, the channel,
  the message and its time, and the outcome (`advanced`, `already-read`, `dry-run`, `skipped`, or
  `failed`). A dry run decides everything and writes nothing, so an operator can check what would be
  marked before turning it on.
* **What it cannot do.** The provider's marker covers the whole channel, so people's messages there
  are marked read too, and Settings says so beside the switch. A provider whose threads have their
  own read state keeps replies inside threads unread, as Google Chat does (above). The first push
  also clears the backlog, and the warning in Settings says that too.
* **Undo.** Turning it off stops all further pushes at once, and so does `DELETE /api/v1/storage`,
  which turns every channel back to off in the running server as well as in the store. Settings shows where the provider's
  marker was before vibe-talk first moved it; moving it back is a manual act in the provider's own
  tools, not a button here, because it would also mark unread whatever the owner has read since.

The bridge implements `POST /channels/{id}/upstream-clear` with exactly
`{"message_id":"...","dry_run":true|false}`. It finds its provider's current cursor, moves it through
that message only if the cursor is behind (never in a dry run), and answers
`{"scope":"space","channel_id":"...","message_id":"...","target":"<instant>","previous":"<instant>|null","advanced":bool,"dry_run":bool}`.
`advanced` says whether the cursor moved, and in a dry run whether it would have. `dry_run` is
required and must echo the request: a bridge that answers a dry run as a real push (or the other way
round) is not asked again for that channel until the setting is saved, because it may already have
marked messages read. It should serve this without making the owner's own actions wait long. `404` means it no longer holds the
message, and vibe-talk waits for a newer one; `409` with `"code":"thread_channel"` means the channel
cannot be cleared at all (for example one thread of a larger space, where the provider's marker would
also clear messages vibe-talk never received), and vibe-talk stops until the setting is saved again;
`429` with `retry_after` in seconds is waited out; anything else is a failure and is retried. An older
bridge answers `405`, which Settings reports as a failure saying the bridge needs updating. Direct Discord is never offered it.

Reading the cursor can be the scarce part. Google Chat allows about two read-state reads per user per
minute, and a bridge that read the cursor fresh before every automatic write, for every channel,
would spend that quota on its own. On this route, for a real push, a bridge may instead keep the
cursor it last read or wrote for each space and read it fresh only when that is several minutes old
or the target is not after it. Two calls always read it fresh instead, because a cached cursor
would make them wrong:

* **The owner's own mark**, `POST /channels/{id}/read` (**Mark read in**). It is rare, and it can
  name any message, however old, so a stale cursor below that message would let it move the cursor
  back behind what the owner has read since. It reads fresh before writing and writes nothing when
  the cursor is already at or past the message.
* **A dry run.** It writes nothing and exists to show what a real push would do, so its `advanced`
  and `previous` must be the provider's own, not a remembered value.

What the cache gives up on the automatic path, precisely. Each write's own target becomes the cached
cursor, so nothing the bridge writes ever lands behind its own earlier writes. What it cannot see,
for as long as a cached cursor is trusted, is anyone else moving the cursor. If the owner reads in
the provider's own client past the message being pushed, which can only be messages newer than it
that vibe-talk has not pushed yet, the push moves the cursor back to its target, and those newer
messages show unread there again until the next push marks them, normally seconds later, when
vibe-talk ingests them. The same goes for a second bridge (or any other client) writing the cursor
of the same space for the same account: this bridge's cache does not see those writes, and can move
the cursor back behind them. So two bridges that cache must not clear the same spaces for the same
account. A spent quota is answered `429` with `retry_after` (and a `Retry-After` header), on this
route and on `POST /channels/{id}/read`, so the server's own backoff waits it out instead of
retrying at once.

### Where it lives

One SQLite file at `storage.path`, which **must be absolute**. A relative path resolves against
the working directory, which in a container is a directory in the *image* — a store like that
works perfectly until the next rebuild and is then gone, with no error anywhere. The server
refuses a relative path at startup rather than discovering this later.

Unset means **off**, and off is loud: the store is a `DisabledStore` that refuses every call and
names the setting to add, and the routes that need it answer `503 storage_not_configured`. It is
deliberately not a silent in-memory substitute, because state that quietly evaporates on restart
is worse than state that was never promised.

### What is kept, for how long

Retention is enforced **on every write**, not by a sweeper that might never run: at most
`max_conversations` conversations (least recently active dropped first), at most
`max_turns_per_conversation` turns in one of them (further turns are refused, not silently
dropped), at most `max_summaries` cached summaries (least recently written dropped first; thread
names are counted separately against the same number), no conversation older than `retain_days`
days since its last turn, and no cached summary, thread sentence or thread name older than
`summary_retain_days` days since it was last **used**: written, or served to a write-token request
more than a day after it was last kept alive. A read-token request keeps nothing alive, because
that would be a durable write reachable from the read token. The defaults are 50, 1000, 2000, 30,
and `summary_retain_days` defaults to whatever `retain_days` is — so a host that lowered
`retain_days` keeps summaries no longer than it did. **`summary_retain_days` should not exceed how
long the chat provider itself keeps messages**: the age limit is what finally removes a summary
whose message was deleted upstream.

**Taking a channel off the list deletes its summaries.** Hiding a configured channel or removing
an added one, in Settings, deletes that channel's cached summaries and thread sentences at once,
from the store and from the running server's memory; its thread names are kept, so the channel
comes back under the names you knew, and its sentences are asked for again.

Two tables are deliberately outside all of it: the "dealt with" marks, which an age bound would
put back into the to-do list purely because time passed, and the **channel aliases**, which an age
bound would rename back months later with nothing on screen saying why. Both are bounded by
something other than time — a count, and the channel allowlist.

The age bound is the only thing that collects an **orphan**: a cached summary whose message was
edited or deleted upstream is unreachable by key, is filed under the current policy version, and
nothing will ever announce that it went. The startup sweep cannot help — it deletes by policy
version, and an orphan is under the live one.

**Deleted means overwritten.** The store sets SQLite's `secure_delete`, so from this version on a
purged or pruned row is zeroed in the file rather than left in a free page for anyone who reads it.
Free pages an older build left behind keep their old bytes until SQLite reuses them or the file is
vacuumed (`sqlite3 FILE 'VACUUM'` with the server stopped). All of this reaches only this file:
**copies of the store that something else made — a backup, or a deploy script that
copies the file before an upgrade — are not reached by the purge**, and have to be pruned where
they were made.

### How an operator purges it

Three ways, all of which erase everything — transcripts, read marks, "dealt with" marks,
cached summaries, channel aliases, the noise rules (back to the default), the voice send setting
(back to the configured default), the owner's instructions for the voice agent (back to the
shipped prompt), and every channel's clear-unread setting (back to off, in the running server too)
alike:

1. `DELETE /api/v1/storage` with the **write** token, on a running server. This is the only
   complete erase over HTTP: `DELETE /api/v1/conversations` clears transcripts and deliberately
   leaves the read marks alone, because the two are different records. It also empties three of
   the copies of channel text held in memory — read-aloud's recent messages, the voice catch-up's
   channel windows with their pushed changes, and the reads a bridge's `recent_view_shadow` holds
   waiting for their check — and a read already running keeps nothing of what it brings back. It
   does **not** empty the other copies in memory, listed below;
2. delete the file (`rm ~/.local/share/vibe-talk/vibe-talk.sqlite3`) — with the server stopped; or
3. delete the mounted directory, which is also what `scripts/run.sh --status` prints.

**The purge erases the store, not everything in memory.** These copies of channel text outlive
`DELETE /api/v1/storage` until they age out, are evicted, or the server restarts:

* the live stream's replay tail: up to 200 published changes per channel, with their text, which a
  page that attaches after the purge is sent (see "The stream");
* read-aloud tickets: each holds one message's text as it will be spoken, at most 512 of them, for
  up to ten minutes;
* thread names and their one-sentence summaries already read from the summary cache: the purge
  erases the rows, but the server goes on answering from what it read before; and
* smaller records, such as the paren tags remembered for headings (`#242 rich-headers`).

So when nothing read from a channel may be left in memory either, restart the server after the
purge.

Unsetting `storage.path` is **not** one of them. It turns the feature off for the next run and
leaves every byte already written exactly where it is — which is the opposite of a purge, and is
worth saying because it looks like one.

### Security posture — this is new, and it is a change

This is the first thing vibe-talk retains at rest, and what it retains is your own speech
plus Discord text written by third parties.

* The database file is created `0600` and its directory `0700`.
* It is **not encrypted**. Anyone with filesystem access to the host — another process running as
  the same user, a backup, a stolen disk — can read every transcript. There is no key management
  here and pretending otherwise would be worse than saying so.
* The bind mount means state now **outlives the container, the image, and a token rotation**. A
  rebuild no longer clears it. That is the point, and it is also the risk.
* Message content still never reaches the access log at INFO; see below.
* Conversation ids are validated against `[A-Za-z0-9_-]{1,64}` before they are used as a key,
  because they arrive from the vendor and from the browser.

### Cached summaries

`src/summarize/` is a second trait over the same store. **Two** implementations ship: one plain
Messages API call per summary when `[summaries.llm]` is configured (see [Summaries](#summaries)),
and otherwise the conversational agent named in `[elevenlabs]`, asked in text. The server says what
is running at startup, and every summary answer carries the summariser's name, so a page can never
imply a model summary it did not get.

There used to be a second one, `extractive`, and it was the default: truncation, no model, no
network, no cost. It has been **deleted**. It never comprehended anything — what it returned was
the opening of the message it claimed to summarise, which is exactly what a collapsed row already
shows you — so it was an intermediate step, not a product.

`summaries.backend` and `VIBE_TALK_SUMMARY_BACKEND` no longer select anything, but both are still
READ, so that a configuration naming the removed summariser is answered rather than ignored.
`extractive` is refused at startup, by name, saying it was removed and what runs instead.
`elevenlabs_agent` — which one release ago was the documented way to ask for real summaries, and is
therefore sitting in live configuration files — still boots, with a loud warning that the key is
dead and should be deleted; refusing it would stop those servers for no behavioural gain, since it
names exactly what they now get anyway. Anything else is refused as unknown.

The consequence is worth stating plainly rather than leaving to be found: **without
`[summaries.llm]`, the ElevenLabs credentials are load-bearing for summaries.** Without either there
is no summariser at all. The
server still starts and says so; `/summary` then answers `503 summarizer_not_configured`, and
`/voice` states the reason once above the list and turns the rows it asked about red. It does not
fall back to truncation, because a silent fallback is indistinguishable from the agent answering
badly. Regeneration also now costs vendor calls where it previously cost nothing.

What is actually load-bearing here is the **cache key**, because a cached derived value has one
failure mode and it is silent. What decides what a summary says is the prompt, whatever else the
backend contributes, every `[summaries]` setting that changes the wording, and the message text.
All but the last are folded into `summarize::policy_version` — one function, one string, part of
every key — so changing any of them makes every summary produced under the old policy unreachable
at once, and a startup sweep deletes the entries. The message text is a separate `content_hash`, so
an upstream **edit** invalidates one entry rather than the whole cache. Settings that change no
word of a summary — the threshold, the reply deadline, the length an answer is cut at — are kept out
of the key on purpose: see [Summaries are kept](#summaries-are-kept).

The backend's own contribution comes from the trait rather than from a constant beside the
constructor: `Summarizer::backend` gives the slug and `Summarizer::policy_input` gives everything
else. That matters for the agent backend, whose answers depend on *which agent* — a different
`elevenlabs.agent_id` is a different model, a different system prompt and a different voice of
writing, and it lives outside `[summaries]` entirely. It goes into `policy_input`, so repointing
the deployment cannot serve the old agent's summaries.

The version is deliberately legible (`v2-elevenlabs-agent-w3-c160-8f1b…`), because a stale entry is
diagnosed by someone with a shell. It is FNV-1a and is **not** a security hash: it detects a
configuration change, and nothing more.

Deleting the extractive summariser moved that string three ways at once — the slug, the policy
input, and the removal of the old `backend` term from the hash — so **the first start after this
change empties the summaries table**. That is the sweep working, not a bug: re-serving an
extractive prefix under an agent key is precisely the silent stale summary the version exists to
prevent. The number swept is logged at INFO at startup and nothing else announces it.

A summariser is a model being fed channel text, so the request goes through `src/untrusted.rs`
exactly as the MCP path does — the instruction outside the fence, the message inside it. That is
structural rather than a convention: `SummaryRequest` holds the built prompt in a private field
and the only constructor builds it through `untrusted::fenced`, so a backend cannot be handed
channel text that was never framed, and a test reads back what the summariser was actually given
rather than rebuilding it. The fence is not decoration here: the summariser really is a model
reading other people's text, so the instruction it is given and the surrounding messages it is
given are two different kinds of input and are framed as two different kinds of input. Being short
is not an exemption.

A cached summary is a second at-rest copy of other people's text under the same file, with the
same `0600`. It is bounded by retention, it goes with the rest on a purge and with its channel when
the channel is taken off the list, and it is never written by a read-scope caller: `/summary` answers a read token and serves it from the
cache, but only a write-scope caller's answer is filed. The read token is the one pasted into the
voice agent, and a durable write reachable from it is what the two-token split exists to prevent.

### Summaries from the ElevenLabs agent

> **Nothing in this section has been run against live ElevenLabs**, and this section is now the
> whole of message summarisation — there is no second summariser and no setting that selects one.
> The protocol was read out of the vendor's SDK. Every offline test drives the real socket client
> against this repository's own mock, which was written from the same reading — so the two agree
> with each other, not with the vendor. The first real conversation is the experiment, and it is
> instrumented to be one.

There is no REST endpoint for "send this text to my agent, get text back". `simulate_conversation`
looks like the missing one and is not: it is deprecated, and what it simulates is the *user*. The
only path is the conversation WebSocket the browser uses, so `src/elevenlabs/socket.rs` opens one,
sends `conversation_initiation_client_data` with `conversation_config_override.text_only = true`
— which suppresses speech synthesis, the expensive half — asks a `user_message`, and reads until
`agent_response`. It answers the vendor's application-level `ping` events while it waits, because
a conversation that stops echoing them is closed for being idle.

**The pool is the design.** A ConvAI socket is a *conversation*: everything sent down one stays in
the agent's context, including the agent's own previous answers, and there is no event that resets
it. Summarising twenty messages down one socket makes each summary an input to the next — context
grows without bound, cost grows with it, and later summaries are written in front of earlier
people's messages. A socket per summary avoids all of that and pays a mint, a TLS handshake, a
WebSocket upgrade and an initiation round trip every single time, on a vendor that bills
connection time as well as tokens.

So there is **one pooled conversation, recycled every `summaries.max_per_socket` summaries
(default 8) and closed after `summaries.socket_idle_seconds` of quiet (default 30)**. Counting in
prompt-equivalents, the k-th summary on a shared socket carries about *k* of them, so a socket
serving *N* costs about *N(N+1)/2* where *N* separate sockets cost *N*: at N=8 that is 36 for 8,
with the handshake paid once instead of eight times; at N=32 it would be 528 for 32 and the
quadratic term has overtaken the handshake it was meant to amortise. Both numbers are folded into
the cache key, because both change what a summary says. A pooled conversation that fails on its
first write is retried **once** on a fresh socket — a vendor is free to close an idle conversation
from its end, and failing a summary for that would make the pool a source of errors the naive
design does not have. A *fresh* socket that fails is a real failure and is not retried.

**Plain text.** The summary is rendered with `textContent` and read aloud, so a markdown bullet is
an asterisk on the screen and a word in the ear. The agent is told so, in `PLAIN_TEXT_RULE`, which
is part of the cache key; and because an instruction is a request a model may decline, what comes
back is flattened — leading heading and list markers, paired `**`/`__`, and everything
`summary::condense` already removes. Deliberately conservative: a single `*` is left alone,
because mangling "the `*` operator" is worse than the bullet that was already stripped.

**It measures itself.** How a round trip to a hosted conversational agent compares with a
full-size model is an open question nobody here has a number for, so the first real run answers it
rather than needing a separate experiment. Every summary answer carries `generated_in_ms`
(`null` for a cache hit or a message below the threshold — a zero would claim the vendor answered
instantly), `ops` logs one INFO line per request with the backend and the elapsed time, and the
agent backend logs its own breakdown: `connect_ms` (what the pool exists to avoid paying),
`answer_ms`, `reused_socket` and `socket_turn`.

The four failures stay apart, as everywhere else on the ElevenLabs side: not configured (names the
setting, dials nothing), refused (the vendor said no — a rejected key at the mint, or a close
frame on the socket), unreachable, and a shape this server cannot read. `summarizer_refused` is
its own code precisely because "the key is wrong" and "the network is down" are fixed differently.
Every error built from a vendor response goes through `redact`, against both the account key **and
the minted URL**, which carries a single-use token and is quoted verbatim by a connect failure.

### Migrations

`src/store/sqlite.rs` holds an append-only `MIGRATIONS` list and records progress in SQLite's
`user_version`. Opening an older file applies the missing steps. Opening a file written by a
*newer* vibe-talk is **refused**, naming the reason — a downgrade that wrote through the old
statements would corrupt data quietly.

## The access log

At `RUST_LOG=info` this server used to log **nothing per request**. That gap has a specific cost,
and it was paid: a voice agent reported that it had read a Discord channel and posted a reply, and
it had done neither — it invented a digest, and it named tools this server does not have
(`file_system_read`, `code_execution_sandbox`, `web_scraper`). The claim could only be disproved
by opening Discord and looking, because "the agent never called us" and "the agent called us and
we answered" produced identical output: none.

So every request now leaves exactly one line, and **an empty log is a finding rather than an
ambiguity**. The server says so in a banner at startup, because a reader has to know the log would
have spoken.

```text
INFO vibe_talk::access: request method="POST" path="/mcp" credential="write" status=200 millis=12
INFO vibe_talk::access: mcp rpc_method="tools/call" credential="write" is_notification=false
INFO vibe_talk::access: tool tool="post_reply" channel="123…" credential="write" outcome="ok" reason="-" text_len=48 millis=9
INFO vibe_talk::access: post_gate event="proposed" proposal=4 channel="123…" text_len=48 reason="-" confirmed_by="-"
INFO vibe_talk::access: post_gate event="committed" proposal=4 channel="123…" text_len=48 reason="-" confirmed_by="ui"
```

Filter for just these: `RUST_LOG=vibe_talk::access=info`. Three lines answer the question that
started this: *did the agent call, which tool, which channel, and was it allowed?*

A tool call the caller stopped waiting for — a voice runtime gives a tool fifteen seconds and then
drops the request — writes `tool tool="…" outcome="abandoned" millis=…` from the moment its handler
is dropped. Before `#210 voice-catchup-speed` such a call left no `tool` line and no `request`
line at all, so the one call that most needed explaining was invisible here.

What a line never contains:

* **No credential.** Not the token, not a prefix, not a hash. Only the *class* that arrived:
  `absent`, `unrecognized`, `read`, or `write`. "Absent" and "unrecognized" are kept apart because
  they are different incidents — a misconfigured client versus a stale token or a stranger.
* **No channel text at INFO.** Message content is written by other people; it does not belong in
  an operator's log or in whatever ships that log onward. A post records its *length*, which is
  what answers "did the whole message go through?". The full arguments of a tool call are
  available at `RUST_LOG=debug` and only there.

A refused request is logged as loudly as an accepted one, including a request to a path that does
not exist — a client pointed at the wrong URL is exactly the case this log exists to catch.

## The API

All `/api/` routes require `Authorization: Bearer <token>`. The two live-adapter routes use their
own adapter-only token; every other route uses the read/write tokens described above.
`/healthz` and the static web app require neither, and neither reveals configuration.

| Method | Path | Scope | Purpose |
|---|---|---|---|
| GET | `/healthz` | none | liveness |
| GET | `/api/v1/channels` | read | configured channels |
| GET | `/api/v1/activity?hours=&per_channel_limit=` | read | every listed channel's last few hours in one read, the newest message named: the route behind `recent_activity` — see "The eight tools" |
| POST | `/api/v1/channels` | **write** | direct mode: `{id,label,writable}`; managed mode: `{source,label}` — validate and add a tracked channel |
| GET | `/api/v1/channel-directory?q=&limit=&cursor=` | **write** | managed mode: one page of the channels the bridge can see, each marked `tracked` when already a channel here |
| DELETE | `/api/v1/channels/{id}` | **write** | take a channel off the list: one added in the app is forgotten (managed mode also unregisters it upstream); one from the configuration file is hidden — see "Taking a channel off the list" |
| DELETE | `/api/v1/channels/{id}/hidden` | **write** | show a hidden configured channel again; `unknown_channel` for anything the file does not name |
| GET | `/api/v1/client-config` | read | what the web app needs at startup, including the caller's own `token_scope`; a write-scope caller also gets `hidden_channels` |
| POST | `/api/v1/live/events` | ingest | accept one normalized create/update/delete event from an external provider adapter |
| POST | `/api/v1/live/hints` | ingest | content-free `{channel_id}` wake-up for that polled provider's existing cursor |
| GET | `/api/v1/diagnostics` | read | re-run the startup checks now, structured, with a remedy on every failure — see above |
| GET | `/api/v1/agent-tools` | read | the voice agent's tool manifest and approval policy |
| GET | `/api/v1/voice-session` | **write** | open a browser-ready session through the configured conversational voice provider |
| GET | `/api/v1/voice-agent` | read | the versioned voice-agent prompt and which MCP tools this credential may call — see "Conversational voice providers" |
| POST | `/api/v1/voice-timing` | **write** | one call's content-free startup phase offsets, logged as `voice_startup` |
| POST | `/api/v1/voice-health` | **write** | a content-free "the voice service is not responding" record, logged as `voice_unresponsive` or `voice_recovered` — see "When the voice service stops answering" |
| POST | `/api/v1/voice-end` | **write** | how a call that opened ended — `hang_up`, `error_frame`, `server_closed`, `connection_lost` or `suspended` — as one content-free line, `voice_call_ended` — see "How a call ended" |
| GET | `/api/v1/signed-url` | **write** | compatibility endpoint that mints an ElevenLabs signed URL |
| GET | `/api/v1/channels/{id}/messages?limit=` | read | full scrollback, oldest first; an optional `as_of` when the provider served a copy it could not bring up to date (`#208 persistent-cache`) |
| GET | `/api/v1/channels/{id}/messages/{message_id}` | read | one message in full |
| GET | `/api/v1/channels/{id}/digest?limit=&width=` | read | one speakable line per message |
| GET | `/api/v1/channels/{id}/messages/{message_id}/summary` | read | one message summarised, from cache when it can be |
| GET | `/api/v1/channels/{id}/page?limit=&before=&since=&until=&hours=&render=` | read | **one step of a walk**, saying that it is one; `render=html` adds each message's `content_html`, which the page asks for and the voice agent's `read_page` does not |
| GET | `/api/v1/channels/{id}/timeline?view=&thread_id=&limit=&before=&after=` | read | main channel, thread list, flattened history, or one thread; opaque backward cursor, or `after=` for what changed since a newest read: `400 cursor_mismatch` for a cursor from another channel, view or thread, `410 cursor_expired` for one the backend can no longer continue — read the newest page again (`#203 incremental-refresh`) |
| GET | `/api/v1/channels/{id}/count?since=&hours=&cap=` | read | a bounded, honest count |
| POST | `/api/v1/channels/{id}/resolve` | read | **semantic random access** |
| POST | `/api/v1/channels/{id}/reply` | **write** | `{text, thread_id?, reply_to?, idempotency_key?}` — post to the channel or selected thread; quoting a message is optional. A post longer than the channel's provider takes in one message is cut into parts; one to the channel itself continues in the first part's thread where the provider threads a long message (Google Chat, Slack) and as consecutive messages on Discord (`#263 post-split-per-provider`). A split post that fails part-way answers `207` with `{posted, unsent, retryable, resumable, thread_id?}`, where `thread_id` names the thread `unsent` belongs in — send it there, and without `reply_to` when that is not the request's own thread; `idempotency_key` (1–64 of `A-Z a-z 0-9 - _`) names the post across attempts, and where client-config reports `idempotent_posts_supported` the same key and text are posted at most once (`#195 send-resilience`). `unsent` is the request's own text when nothing posted; `resumable` says whether sending `unsent` again repeats the failed part under the same words and key, which a split code block can prevent. Posts to one destination — the channel, or one thread — are made one at a time, in the order they arrive, a long post's parts together; a post whose caller disconnects still holds its destination until the provider answers or its request timeout gives up (`#277 send-order`) |
| POST | `/api/v1/channels/{id}/ask` | **write** | slow path — answers 501 in v0 |
| GET | `/api/v1/post-proposals?wait=&seen=` | **write** | the one post waiting for the owner's confirmation, or `null`; `wait` (seconds, at most 25) holds the request until it changes from serial `seen` — see "Posting asks first, unless you turn that off" |
| POST | `/api/v1/post-proposals` | **write** | `{channel_id, text, reply_to?}` — what `post_reply` does: propose a post, superseding any pending one, and answer `{sent: false, serial, result}`, where `result` is `post_reply`'s sentence, and no handle; or, with "Ask me before the voice agent sends" off, post it at once and answer `{sent: true, serial, result}` (207 with the unsent remainder, and the thread it belongs in, as the reply route answers, when only part went; the same words again within a minute post nothing, answering the first send's `{sent: true}` when it posted and `409 post_repeated` when it may have) — see "Posting asks first, unless you turn that off" |
| POST | `/api/v1/post-proposals/commit` | **write** | `{handle, channel_id, text, reply_to, confirmed_by}` — confirm and post it, once |
| POST | `/api/v1/post-proposals/cancel` | **write** | `{handle}` — withdraw it; `204` |
| GET | `/api/v1/channels/{id}/stream` | read | **Server-Sent Events**: messages as they arrive — see "Live push" |
| GET | `/api/v1/conversations` | **write** | stored `/voice` transcripts, most recent first |
| DELETE | `/api/v1/conversations` | **write** | erase every stored transcript |
| GET | `/api/v1/conversations/{id}` | **write** | one stored transcript, oldest turn first |
| DELETE | `/api/v1/conversations/{id}` | **write** | erase one stored transcript |
| POST | `/api/v1/conversations/{id}/turns` | **write** | record one turn |
| GET | `/api/v1/conversations/{id}/replay` | **write** | that transcript, budgeted and fenced, for a new call — see "Resuming" |
| GET | `/api/v1/inbox` | read | how far this server thinks each channel has been read |
| GET | `/api/v1/channels/{id}/todo?limit=` | read | the recent window MINUS what has been dealt with; `window` and `complete` say how big the window was and whether it is the whole channel, and `window_from` when its oldest message was sent |
| POST | `/api/v1/channels/{id}/dismiss` | **write** | `{messages:[…]}` (at most 1,000 ids), or `{through:id, limit:n}` — mark as dealt with; answers the exact set it changed. Send back the `limit` you read `/todo` with, or `through` is resolved against the default window and clears more than you displayed |
| POST | `/api/v1/channels/{id}/restore` | **write** | `{messages:[…]}` (at most 1,000 ids) — the undo, restoring exactly that set |
| POST | `/api/v1/channels/{id}/read` | **write** | move this server's read mark forward |
| DELETE | `/api/v1/channels/{id}/read` | **write** | drop this server's read mark |
| POST | `/api/v1/channels/{id}/upstream-read` | **write** | `{message_id:"…"}` — ask an explicitly capable provider to move its monotone read cursor through that message; thread behavior is provider-defined |
| GET | `/api/v1/channels/{id}/upstream-clear` | read | whether this server clears the channel's unread state in the provider's own client as it ingests messages, and what it last did; see "Clearing unread automatically" |
| PUT | `/api/v1/channels/{id}/upstream-clear` | **write** | `{mode:"off"\|"dry_run"\|"on"}` — set it. Turning it on pushes nothing by itself; the next message that arrives is pushed. Refused for a channel whose provider cannot do it |
| POST | `/api/v1/channels/{id}/upstream-clear/now` | **write** | no body — Settings' **Mark read now**: push the channel's newest message at once through the running loop and answer `{outcome, message_id?, message_at_ms?, attempt?, state}`: `outcome` is `pushed` (the provider took the push that covers the pressed message; `attempt` is it), `failed` or `skipped` (that push failed, or named a message the bridge no longer holds; `attempt` says so), `covered`, `no_message`, `retry_waiting`, `changed` (the setting changed meanwhile), `unavailable`, or `waiting` (nothing recorded within 45 s, the whole request included; the server carries on). Changes no setting; refused while the channel is off |
| PUT | `/api/v1/channels/{id}/alias` | **write** | `{alias:"…"}` — what THIS app calls the channel; see "What to call a channel" |
| DELETE | `/api/v1/channels/{id}/alias` | **write** | drop it, putting the configured label back |
| PUT | `/api/v1/noise-rules` | **write** | `{rules:["…"]}` — replace the whole list of messages read automatically; answers what was stored. Read through client-config's `noise_rules`; see "Messages read automatically" |
| GET | `/api/v1/voice-settings` | read | `{ask_before_send, changeable}` — whether the voice agent asks before it sends, and whether this server can keep a change; also client-config's `voice_settings`. See "Posting asks first, unless you turn that off" |
| PUT | `/api/v1/voice-settings` | **write** | `{ask_before_send: bool}` and nothing else — the owner's choice, kept in the store; answers what was stored; 503 `storage_not_configured` with no store. No MCP tool reaches it |
| GET | `/api/v1/voice-prompt` | read | `{instructions, system_prompt, system_prompt_edited, edited_from_version?, prompt_version, max_instructions_chars, max_system_prompt_chars, changeable, delivery}` — the owner's additional instructions and the base prompt in force (his copy or the shipped one); also client-config's `voice_prompt`. See "Instructions for the voice agent" |
| PUT | `/api/v1/voice-prompt` | **write** | `{instructions?, system_prompt?}`, at least one — save either text; an absent field keeps its text, an empty `instructions` removes them, and an empty `system_prompt` (or the shipped one) restores the shipped prompt. Answers what was stored; 413 `too_large` over 4,000 / 20,000 characters; 503 `storage_not_configured` with no store. No MCP tool reaches it |
| POST | `/api/v1/channels/{id}/not-noise` | **write** | `{messages:[…]}` — these are real messages; stop reading them automatically, leaving the rules alone |
| GET | `/api/v1/channels/{id}/pins` | read | this app's pinned messages in the channel, oldest message first, each with the snapshot kept of it, and the list's `revision`; see "Pinned messages" |
| PUT | `/api/v1/channels/{id}/pins/{message_id}` | **write** | `{author, author_id, author_is_bot?, content, timestamp, thread_id?, thread_root?}` — pin it, keeping that much of the row; idempotent. At 100 pins in the channel the oldest is unpinned and named in `unpinned` |
| DELETE | `/api/v1/channels/{id}/pins/{message_id}` | **write** | unpin it; idempotent, `changed: false` when it was not pinned |
| GET | `/api/v1/channels/{id}/bookmark` | read | the channel's reading bookmark, `{revision, anchor}`, `anchor` null when none is set; every channel read carries it too. See "The reading bookmark" |
| PUT | `/api/v1/channels/{id}/bookmark` | **write** | `{message_id, timestamp, place?:"above"\|"below", thread_id?, if_revision?}` — put it next to that message; a stale `if_revision` is `409` with where it is now; idempotent |
| DELETE | `/api/v1/channels/{id}/bookmark?if_revision=` | **write** | take it away; `if_revision` as for PUT; idempotent |
| DELETE | `/api/v1/storage` | **write** | erase EVERYTHING durable: transcripts, read marks, "dealt with" marks, cached summaries, channel aliases, pins, bookmarks, the voice send setting and instructions, and every channel's clear-unread setting (back to off) |
| POST | `/mcp` | read, or **write** per tool | MCP over Streamable HTTP — see below |
| GET/DELETE | `/mcp` | none | `405`; this endpoint is stateless and has nothing to push |

**The transcript routes require the write scope even to READ.** A stored transcript is your
own speech plus whatever channel text the assistant read out to you, which is a different
and more sensitive thing than a digest of a channel you already allowlisted — and `/voice`, the
only thing that uses them, already holds the write token. The read token gets `403`, and a test
asserts it. **None of them is an MCP tool**, and a test holds that line too: text a tool can
return is text that enters a model's context.

**No read-scope credential writes anything durable.** That is the wider rule the line above is one
case of: every durable write — a turn, a read mark, a cached summary — needs the write scope.
`/summary` is the interesting one, because it is readable at read scope and yet has something to
file: a read token is served from the cache when there is a hit and its answer is never written
back. `/inbox` is the single durable READ a read token may make, because how far you have
read is the thing the voice agent has to be able to say out loud.

**`/api/v1/inbox` carries `read_state_notice` on every answer**, including the local mutating ones,
saying that this read state is vibe-talk's own and is synchronised with Discord in neither
direction. `/upstream-read` is a separate provider write, advertised to the web app through
`upstream_read_mark_supported` in `/client-config`, and `/upstream-clear` is advertised through
`upstream_clear_supported`, per provider as well. See **Durable state** above.

**Every message this API renders carries `author_id` next to `author`** — in the scrollback, in
one-message reads, in `resolve` results, in the posted message echoed back by `reply`, and as a
field on every digest entry. That is what makes a real Discord mention possible: `@coding_agent`
typed as words notifies nobody, and only `<@1532416065114607829>` does. Bots have ids too, and
they are included, because addressing another coding agent is a legitimate reply.

**A page says that it is a page.** `GET .../page` answers with `returned`, `has_more`, and —
when there is more — `next_before` or `next_since`, whichever continues the walk it was asked
for. Two modes, and they cannot be mixed: `before=<id>` steps backwards from a cursor, and
`since=`/`until=` (ISO-8601, start inclusive, end exclusive) jumps to a period. `hours=` (1 to 168)
is the second mode with its start computed on this server's clock — from that many hours ago to
now — so a caller asked about "the last day" need not learn the time first; it stands alone, and
`invalid_range` answers it combined with any of the other three. `count` takes it too, instead of
`since=`. A page tops out at
99 rather than 100 on purpose: the server fetches one more than it returns and drops it, which is
the only way to answer "is there more?" exactly instead of guessing from a full window.

Ranges work because a Discord snowflake carries its own creation time in its top 42 bits, so an
instant converts straight into a cursor. A numeric offset would have to be synthesised and would
drift as messages arrive, so there is deliberately no `offset=`.

**A count is bounded and says when it is a floor.** Discord publishes no message count for a guild
text channel, so `GET .../count` walks backwards a hundred at a time until the channel runs out or
`discord.max_count_scan` stops it, and sets `at_least` when the ceiling was what stopped it.
"At least 500" is the honest answer; a confident total would be either slow or wrong. This is the
defect the count tool exists for: asked how many messages a channel held, an agent answered
**100** — the size of the page it had been handed.

**Every message also carries two times, and only one of them is meant to be spoken.**
`spoken_time` is the instant already converted into `server.timezone`, with no seconds and no zone
label — `09:51` — and it is what a voice agent reads out, verbatim, with no conversion of its own.
`timestamp` is the exact ISO-8601 instant Discord reported, unrounded, and it is what anything
computing with a time must use. The conversion happens once, in `src/ops.rs`, so the phone and the
voice agent cannot disagree about when something was said. This exists because handing an
assistant `13:51:25+00:00` and letting it do the arithmetic produced *"thirteen fifty-one Eastern
Time"* — the right clock with the wrong label, when nine fifty-one was the answer.

The seconds and the label were there at first and were both removed on report: they are the two
parts a listener cannot use. Nobody places a chat message to the second, and the label names the
zone they are already standing in — "nine fifty-one and twenty-five seconds Eastern Daylight Time"
spends its length saying so. What replaces the label as a defence is that a bare `09:51` offers no
zone to reinterpret, and the tool descriptions say in as many words that the field is already
local. `src/clock.rs` records what that trades away.

**And a third field, `spoken_content`, carries the BODY rewritten for speech.** Same rule as the
time: `content` is what was typed and what the screen shows, `spoken_content` is what a voice says,
and the rewrite happens once, in `src/ops.rs`, so nothing downstream computes one. `src/speakable.rs`
does three things to it, in an order that matters. Markdown comes off, because read aloud
`**deploy**` is *"asterisk asterisk deploy asterisk asterisk"*; a link reads as its words, and an
address is said as what it is rather than spelled (`#187 speakable-ids-urls`) — a GitHub page as
the page, *"widgets pull request 41"* for the address the screen shows as `widgets#41`, and any
other address by its host, *"a link on example.com"*, never *"https"*. Timestamps become *"three
hours ago"*, because an ISO string is read out character by character. And long numeric ids and
hash codes get short spoken names. A long numeric identifier becomes *"UID ending in 009"*, a diff
such as `D1234567890` becomes *"diff ending in 890"*, and a task `T1234567` *"task ending in
567"*. Hashes still get letters, so the same hash is the same letter every time it appears. Times
are resolved before ids because a Unix epoch and a snowflake are both long runs of digits and only
one of them is a time. `src/speakable.rs` and `src/link_labels.rs` give every spoken form.

The hash-letter table lives on the server for as long as the server does, shared by every reader,
and is bounded at 4096 names — past that a value is spoken as *"a hash code"* with no letter,
which is audibly different from a named one rather than quietly ambiguous. Sharing one table
across readers is safe **on this deployment** and not in general: there is one channel
allowlist and everyone reads through it, so a named value is always one already on the reader's
screen. A deployment where two readers saw different channels would need two tables.

`spoken_content` is **empty when the rewrite changed nothing**, which is most chat, so an ordinary
sentence is not sent twice down a phone's connection. Empty therefore means both "nothing prepared
this" and "preparing it was the identity" — deliberately, because `Message::spoken_body()` answers
both with the raw body and no reader needs to tell them apart. That is also what makes
`speakable.enabled = false` a one-line switch: off, the field stays empty and every reader is
already correct.

There is deliberately **no user-lookup tool**, and the reason is recorded in `src/model.rs`
beside the field. First, the id arrives ATTACHED to the message being replied to, so there is no
lookup step, nothing to search for, and nothing for a model to hallucinate — a wrong snowflake
pings a stranger. Second, it bounds the capability: the only ids that ever exist here belong to
people and bots that have actually spoken in an allowlisted channel, whereas a lookup tool would
let the voice agent ping anyone in the server.

`POST .../reply` authorizes exactly the users the text itself mentions
(`allowed_mentions: {parse: [], users: [...]}`). `@everyone`, `@here`, and role mentions remain
unreachable, and an empty `allowed_mentions` — which is what this used to send — would have made
`<@…>` render as a mention that silently notified nobody.

`resolve` takes `{"query": "...", "limit": 50, "max_alternatives": 3}` and answers with `best`
(the full message, or `null`), `alternatives`, and `ambiguous`. **A query that matches nothing
returns `best: null`** rather than the newest message wearing a confident label; a voice interface
that guesses is worse than one that says it did not find anything.

### The wire contract

The JSON the server and `/voice` exchange is defined once, as Rust types in `src/contract.rs`
(plus the model types they carry). Everything else is generated from them and checked in:

| File | Generated by | Used for |
|---|---|---|
| `contract/vibe-talk.schema.json` | `tests/contract.rs`, from the Rust types via `schemars` | the reviewable JSON Schema (2020-12) |
| `contract/samples.json` | `tests/contract.rs`, serialized by the Rust types | every union variant, every optional field present and absent |
| `web/contract.js` | `contract/generate.mjs`, Ajv standalone | runtime validators the page loads as `/contract.js` |
| `web/contract.d.ts` | `contract/generate.mjs` | declarations for `tsc --checkJs` over `web/voice.js` |

The roots are the client-config answer, the voice-session mint, the timeline page, the API error
body, the three live-stream events, both directions of `vibe-talk-v1` frames, and the `Message` and
`ThreadSummary` rows the page saves on the device. The page validates each of those where it
arrives — an HTTP answer, a stream event, a WebSocket frame, a row read back from device storage —
and nowhere else.

- A **malformed HTTP answer** is refused with "vibe-talk returned a malformed answer — … Reload
  the page". The page and server ship in one binary, so a mismatch means a stale open tab or a
  server bug, and a guess would hide either.
- A **`vibe-talk-v1` frame with an unknown `type`** is ignored, as the protocol says; a frame whose
  `type` is known but whose shape is wrong is shown as an error and counted as the peer's error
  (`error_frame` in voice health), so a refused `turn_complete` is never mistaken for silence.
- A **stream event** that does not match is treated as a hint to re-read the channel.
- A **saved row** that does not match discards that saved snapshot, which is then re-read.

Changing a wire type:

```bash
npm ci --prefix contract        # once: the pinned Ajv and TypeScript, from contract/package-lock.json
make contract                   # regenerate the schema, samples, validators and declarations
make contract-check typecheck   # what validation runs: stale generated files, then tsc
```

`cargo test --test contract` fails when the schema or samples no longer match the Rust types, and
`make contract-check` fails when the browser files no longer match the schema. Serving the page
needs none of this tooling: `web/contract.js` is plain JavaScript with no dependencies.

## Resuming a conversation across a hang-up

The vendor cannot resume a conversation once the socket closes — the initiation message and the
signed-URL endpoint both take an `agent_id` and neither accepts a `conversation_id`. So this
server does not resume anything. What it does instead is hand a NEW call a written record of the
old one, so the agent can carry on from it.

**That is a reconstruction, and every part of this is arranged so it cannot read as more than
one.** `replay.enabled` is off by default; when it is on, `GET /api/v1/conversations/{id}/replay`
renders the stored transcript and the page sends it on the new socket.

### The states, which are that many different sentences

The failure to avoid is not that this breaks; it is that it half-works and the screen keeps saying
it worked. So the clause under the large control is derived rather than written:

| State | What the screen says |
|---|---|
| off, here or on the server | "the agent starts fresh" |
| armed, whole record sent | "the earlier conversation is replayed" |
| armed, budget dropped some | "the earlier conversation is replayed **in part**" |
| the fetch failed | "the agent starts fresh — the earlier conversation could not be read" |
| nothing was said in it | "the agent starts fresh — there was nothing to replay" |
| the budget dropped ALL of it | "the agent starts fresh — the earlier conversation was too long to replay" |

**The last two are the same `included: 0` and they mean opposite things.** Both send nothing, and
only `dropped` tells them apart. "There was nothing to replay" said about a conversation that was
merely too large is a claim about what the reader said earlier, made with no basis for it — so the
page reads `dropped` as well as `included`, and Settings names the two budget settings that would
fix it rather than leaving the reader to conclude their conversation vanished.

The end-of-call seam changes with it. Every version of it asserted that the agent below the line
has never seen anything above it, and that stops being true the moment resuming is armed; with it
on, the seam says the new call will be read the lines above and that this is a reconstruction, not
the same conversation. A failed replay never aborts the call — it degrades to a fresh one and says
which it was.

### The budget, and why it is stated rather than discovered

A transcript grows without bound, the payload is billed per call, and the model's window is
finite. The rule: keep the most recent turns until either `replay.max_chars` or `replay.max_turns`
is reached, drop **oldest first**, and report how many were dropped. When anything was dropped the
agent is told so inside the payload as well — otherwise it insists the user never mentioned
something the user definitely did.

`0` is refused rather than read as "no limit": a budget of nothing is a replay that is always empty
while the interface says resuming is on, which is a feature that is silently doing nothing.

### The transcript is untrusted text

A stored turn is your own speech AND whatever channel text the agent read aloud to you,
which is third-party Discord text. Every turn is neutralized and the whole record is fenced by
`src/untrusted.rs`; the preamble sits OUTSIDE the fence, because it is this server speaking and
the point of the fence is that nothing inside it is. A turn that forges the fence is defused and
the tampering stays visible.

### Privacy

**Every new call re-sends earlier conversation content to ElevenLabs**, including Discord text
written by other people that the agent read out. That is why it is off by default, why the
Settings screen says it in those words, and why it deserves an explicit decision if any of that
content is sensitive.

### The one check that can answer the real question

Whether the vendor puts a `contextual_update` sent immediately after the initiation message in
context for the **first agent turn** is a property of the platform. Nothing in this repository can
settle it:

```sh
scripts/run.sh --smoke-agent --replay-check
```

Three conversations, so three times the cost. A states a nonce and its turns are recorded through
this server's own transcript API, exactly as `/voice` records them; B opens WITH that record and
must return the nonce; **C is the control** — same question, no record — and must NOT be able to
answer, or the run proved the agent is fluent rather than that it remembers. All three outcomes
are reported separately, and "the vendor did not honour it" has an exit code of its own (21)
rather than being folded into a generic failure. It refuses, billing nothing, when this server has
no durable store.

**Until that check comes back green on a deployment, the interface must not claim a call was
resumed there.** `replay.transport = "client_data"` exists so the other path can be measured too:
it carries the text on the initiation message under `dynamic_variables`, which depends on the
agent's dashboard security settings permitting overrides and fails SILENTLY when they do not.
That variable only reaches the agent through a `{{vibe_talk_resume}}` placeholder in the prompt
pasted into its console, so on a call where the page replaces that prompt (`#272
voice-agent-instructions`, "Instructions for the voice agent") the placeholder is gone with it: the
page then sends the record as a `contextual_update` as well, straight after the initiation, and
keeps the variable on the initiation so nothing configured at the vendor goes missing.

## Live push

Everything else here is pulled: a question arrives, a channel is read, an answer goes back. This
is the exception, and it exists for two things — the channel view on `/voice` should update as
messages arrive rather than when something happens to poll, and a reply that lands mid-conversation
should be able to reach the voice agent on its own instead of waiting to be asked about.

There are two mutually exclusive ways to PRODUCE messages in the same channel-keyed `LiveHub`:

* a provider's `live_poll_seconds` keeps the built-in authoritative polling source;
* `ingest.token` enables an external provider adapter to push normalized events to
  `POST /api/v1/live/events`.

A polled provider may additionally receive content-free change hints at `POST /api/v1/live/hints`.
That is not a third producer: the hint wakes the poller's own provider client and its one cursor.

The browser learns `live_delivery: "off" | "poll" | "push"` from `/api/v1/client-config`, so a
push-only deployment with a zero poll interval still attaches its SSE stream. In push mode the
screen distinguishes "configured and connected to this server" from adapter health, which the
server cannot observe. Full pushed events are refused per channel when that channel's provider is
polled: duplicate delivery must not depend on two unrelated cursors happening to agree. Hints are
allowed beside polling precisely because they contain no message and create no second cursor.

### Change-hint acceleration

Some event sources are excellent wake-ups but incomplete message-create feeds: they omit bot
posts, retain only a short replay window, or expose a payload shape the chat provider already
knows how to normalize better. For those, send only:

```json
{"channel_id":"C0123ABCD"}
```

to `POST /api/v1/live/hints` with the ingest bearer token. The channel must be allowlisted and its
provider must have a nonzero `live_poll_seconds`; otherwise the route answers `404 unknown_channel`
or `409 channel_is_not_polled`. A newly queued hint answers `202`; another already pending hint
answers `200` with `{"accepted":false,"duplicate":true}`. Pending hints for one provider and
channel coalesce, including a short 100 ms burst window. The first hinted read is immediate after
that window; while hints keep arriving, later reads are spaced at least five seconds apart, the
same safety floor as configured polling. Each read advances through every create since the shared
cursor, so event volume does not require one provider request per event.

The resulting provider read uses exactly the same cursor as the periodic tick. It therefore cannot
publish a new message twice, and a missed hint merely postpones delivery until reconciliation.
Provider failure backoff still wins over a hint, so a noisy event stream cannot turn an upstream
outage into a request storm. Catch-up events are safe too: the poller seeds its cursor from current
history on startup and publishes none of that seed as new.

Once the adapter is running, make `live_poll_seconds` the desired recovery bound rather than the
ordinary delivery latency (for example, `3600` for hourly reconciliation). Do not set it to zero:
the hint deliberately has no second provider client or cursor of its own to consume it.

`scripts/live-hint-adapter.py` supervises an external JSON-lines event command and turns each JSON
object it emits into one such hint. It invokes the command literally, without a shell; caps each
record; reads the ingest credential from a private file; retries only transport failures, `429`,
and `5xx`; and exits on a permanent `4xx` or malformed stream so a service manager can restart or
surface the fault. The event command owns event filtering: do not enable heartbeat output, because
every valid JSON object is intentionally treated as a change hint.

Run its offline protocol and privacy controls before installing it:

```sh
python3 scripts/live-hint-adapter.py --self-test
```

For a systemd user service, copy
[`systemd/vibe-talk-live-hint.service.example`](systemd/vibe-talk-live-hint.service.example) to
`~/.config/systemd/user/vibe-talk-live-hint.service`, replace its paths, channel, URL, and event
command, and put the same value as `ingest.token` in `~/.config/vibe-talk/ingest-token` with mode
`0600`. The example binds the adapter lifecycle to `vibe-talk.service`, treats even a clean event
stream EOF as a failure, and restarts it without making the channel writable:

```sh
systemctl --user daemon-reload
systemctl --user enable --now vibe-talk-live-hint.service
```

The adapter intentionally fixes one channel on its own command line, so an event cannot choose a
different allowlisted destination. Install one unit per channel when the upstream source cannot be
split that way itself.

The checked-in service is specifically a **Meta CLI tail** example, not a statement of the generic
adapter's minimum resources or sandbox compatibility. On a devserver, Meta's managed runtime
mounts its packaged client and settles above 512 tasks and 1 GiB; it also aborts when systemd gives
it `NoNewPrivileges` or `PrivateTmp`. The example therefore leaves those two directives unset and
allows 2 GiB and 2,048 tasks, matching the same host's established Meta-backed Slack shim. The
Python adapter itself does not need that footprint. If the command after `--` is not Meta CLI,
measure that command and restore the strongest sandbox and smallest limits it supports rather than
copying Meta's exceptions without evidence.

### Provider-adapter push

The adapter route has its own bearer token, distinct from both browser tokens. It accepts only
allowlisted channel ids, limits each JSON body to 64 KiB, and remembers the newest 1,024 event ids
per channel in memory. A retry is idempotent only while its id remains in that window in the same
server process: a restart or 1,024 later events permits it again, so consumers must still tolerate
an occasional duplicate. An accepted event answers `202`; a known retry answers `200` with
`{"accepted":false,"duplicate":true}`. A producer retries only a transport failure, `429`, or `5xx`,
never a permanent `4xx` refusal.

Delivery order is request-acceptance order. The adapter must keep at most one request per channel in
flight, wait for its response, and settle an uncertain result before sending the next event for that
channel. Event ids are opaque de-duplication keys, not sequence numbers, so concurrent requests
cannot be put back into provider order by the server.

Every event carries an opaque, stable `event_id` and a mandatory `historical` boolean. The latter
is a safety boundary: subscription catch-up is rendered but leaves the SSE payload's `replayed`
flag true, so old channel text is never announced to a live voice conversation as news. A producer
must say `historical: false` only when it knows the event arrived after its live subscription was
established.

```json
{"event_id":"provider-event-42","historical":false,"kind":"create","message":{"id":"9001","channel_id":"123","author":"Ada","author_id":"7","author_is_bot":false,"timestamp":"2026-09-18T12:00:00Z","spoken_time":"","reply_to":null,"content":"done"}}
```

Updates use `kind: "update"` with the complete current message. Deletes use
`{"event_id":"…","historical":false,"kind":"delete","channel_id":"123","message_id":"9001"}`.
They become `event: message_update` and `event: message_delete`; the page re-reads the authoritative
window while preserving its scroll position, rather than leaving a stale row or trying to patch a
derived combined row in place. Creates remain `event: message`, so existing polling producers and
older clients keep their wire contract.

SSE resume ids are event ids, not message-order cursors. Adapter ids are namespaced with `push:` on
that internal wire so even a numeric provider event id cannot be mistaken for a numeric message
cursor. If `Last-Event-ID` is still in the bounded tail, the hub replays exactly what followed it,
including edits and deletes. If an opaque id has fallen out, the server emits `event: reset` and
ends the stream; guessing a partial replay would hide a gap. The same rule applies to polling's
numeric ids: numeric comparison cannot prove that the bounded tail did not evict an intervening
message.

### Built-in Discord ingestion is bounded polling, not a Gateway connection

Discord's real-time mechanism is the Gateway. **This server does not use it.** With
`discord.live_poll_seconds` set, a background task reads each allowlisted channel on that interval
and publishes whatever is newer than a per-channel snowflake cursor.

That is deliberately the less capable option:

* **A Gateway is three new things at once.** A WebSocket client for Discord (there is one for
  ElevenLabs and none for Discord), a heartbeat / resume / session-invalidate state machine, and
  the privileged `MESSAGE_CONTENT` intent — which the application has to be granted, and without
  which every message arrives with an empty body.
* **Polling reuses what is already tested.** `DiscordClient::fetch_page`, `sort_oldest_first`
  and `MessageId::numeric` are the whole mechanism, and all three already have tests — including
  the string-versus-numeric snowflake ordering trap, which is exactly the bug a hand-rolled cursor
  reintroduces, and the `before`/`after` asymmetry, which is the other one.
* **The fake can genuinely fail.** `FakeDiscord::fail_next` and its unknown-channel refusal mean
  the poll loop's failure path is exercised by the suite rather than reasoned about.

And one reason that is about sequencing rather than design: **the Discord layer here has still
never run against live Discord.** Making first contact and introducing a stateful always-connected
client in the same change is two untested things at once, and when it misbehaves there is no way
to tell which one is wrong.

**A Gateway remains an upgrade path behind this same seam.** Everything above `src/live.rs` sees a
`LiveHub` — a channel-keyed publish/subscribe with a bounded replay tail. External adapters already
publish into it; a native Gateway implementation would do the same.

Three rules in the loop are correctness, not tuning:

* **The first tick seeds the cursor and publishes nothing.** Otherwise the first poll after a
  restart republishes the whole recent window, and a page that attaches a second later is shown
  existing history labelled as newly arrived — and relays it into a paid conversation as news.
* **A failed fetch does not advance the cursor** past anything it did not publish. Whatever
  arrived during the outage is published once it recovers, rather than skipped.
* **Every tick after the first walks FORWARD from its cursor**, with Discord's `after`, rather
  than re-reading the most recent `default_fetch_limit` messages. Re-reading the newest window and
  then moving the cursor to the newest of them drops everything in between the moment a channel
  produces more than one window between two ticks — silently: no gap event, no warning, and a page
  and an agent simply missing lines. Walking forward cannot lose them.

**And a bound on catching up, because "read until caught up, whatever it costs" is a request storm
against a shared rate limit — and obeying `Retry-After` buys time, not quota.** One tick walks at
most four pages per channel — 200 messages at the default — and stops. Nothing is skipped: the cursor only ever moves past
what was actually published, so the next tick continues where this one stopped. Being stopped by
that ceiling is logged **once**, at WARN, on the way into it, and the recovery is logged too;
delivery is running late and that is worth seeing, but it is lateness, not loss.

**A failing channel backs off** by doubling up to sixteen intervals, and that is pinned by a test
that measures how long the loop actually waited against a healthy control. It has to be measured,
because deleting the line that applies the backoff changes nothing else observable: the loop still
fetches, still publishes, still recovers, still logs.

**And when the failure is a rate limit, the backoff does not undercut it.** A 429 during a tick is
first waited out by the client itself (see "Discord rate limits"); what reaches this loop is a
limit that did not clear inside that budget, still carrying Discord's outstanding `retry_after`.
The loop waits the larger of its own doubling and that number, so the two are one mechanism at two
scales rather than two rival opinions about when to come back. That is measured too: after a
single failure the doubling alone says twenty seconds, which is not long enough for a limit that
asked for forty-five.

### The page keeps the ElevenLabs conversation socket

The server relays nothing to the vendor. It mints a signed URL and that is the end of its
involvement; the browser holds the conversation. Moving that socket server-side would turn
vibe-talk into an always-connected, **billed** conversation holder whose cost accrues while nobody
is in the car, and would put third-party channel text on a vendor socket no human is looking at.

**The cost of that decision, stated plainly: contextual updates reach the agent only while the
page is open.** Close the tab and the channel keeps moving, this server keeps ingesting, and the
agent hears nothing until somebody opens `/voice` again and asks.

### The stream

`GET /api/v1/channels/{id}/stream` is Server-Sent Events, and is an ordinary read in every other
respect: same bearer token, same read scope, same channel allowlist. A channel outside the
allowlist gets `404 unknown_channel`, never a `200` that streams nothing — on a stream those two
are indistinguishable forever.

* Every event carries `id: <message id>` and `event: message`, and a payload of
  `{message, self_posted, replayed, from_tail, untrusted_content_notice}`. The notice is the same
  one `/messages` carries: a pushed message is third-party text exactly as a fetched one is.
* **`replayed` says whether the event came out of the tail or off the wire just now**, because on
  the wire those are otherwise identical. Every attach that carries no `Last-Event-ID` — a fresh
  sign-in, a channel change, the reconnect after an `event: reset` — is handed the whole tail, and
  a page that could not tell would announce up to two hundred old messages to a live conversation
  as news. An adapter's catch-up is `replayed` too. See "What the page does with it".
* **`from_tail` says the event was published before this stream attached**, so any channel read
  begun after the stream's response arrived has already seen it. An adapter's catch-up arriving
  after the attach is `replayed` without being `from_tail`: it may be newer than every read the
  page has made.
* **Reconnection.** The browser sends `Last-Event-ID`; the server replays only what came after it,
  from a bounded tail of the last 200 published messages per channel. The receiver is attached
  *before* the tail is read, under one lock, so nothing can slip between the two.
* **Falling behind.** A subscriber further behind than the broadcast buffer gets one
  `event: reset` and the stream **ends**. The page re-reads through the paged route rather than
  resuming short: a silent gap is the one outcome this design must not produce.
* `Cache-Control: no-store`, and `X-Accel-Buffering: no` so an nginx-shaped proxy does not buffer
  the response into nothing.
* **In the access log it leaves exactly one line, at attach, with `millis=0`.** The middleware
  returns as soon as the status is known, which for a streaming body is before the first event.
  A stream held open for an hour still logs zero. That is pinned by a test so nobody reads it as
  an instant request.

### What the page does with it

`/voice` reads the stream with `fetch` and a reader over the response body, **not** `EventSource`:
`EventSource` cannot carry an `Authorization` header, and the usual workaround puts a bearer
credential in a query string — in every proxy log and in the browser's own history, which is the
exact thing `/api/v1/signed-url`'s `no-store` and the page's `redact()` exist to prevent.

Arriving messages are rendered by the same element construction as fetched ones, de-duplicated
against the rows actually on screen, and — in either of the two speaking modes — **spoken** into a
live conversation as a `user_message`.

**A replay costs a read only when no read has seen it.** On a threaded channel a live message
re-reads the active view, because the server owns thread membership and counts. A replayed message
does not when the store already holds it: the replayed copy is the message as first published, so
the store's copy is at least as new and is kept, and a row the to-do filter hid stays hidden. Nor
does it when a read that post-dates the message has landed: for a message `from_tail`, any read
begun after the attach; for an adapter's catch-up, only one begun after it arrived. A read that
failed saw nothing. Otherwise the message waits for the next read to land. With none on the wire,
the channel pane starts one, which merges it and the rest of its burst; the voice pane starts none,
because entering the channel reads it. A read already out when the burst arrives — a channel change
reads before its stream attaches — is asked, when it lands, which of the burst its page contains,
and one more read follows only for the rest. A reply in another thread is never on the main view's
page, so it costs that one more read. A failed read queues nothing: what waited on it waits for the
poll's read. A warm reload replaying its tail makes no channel read on the voice pane and only its
entry read on the channel pane; a reload that reopens on the channel holds that read until the
stream has attached, so it answers for the whole tail. An edit or removal from the tail waits the
same way, for a read begun after the attach, because a page's copy of a message says nothing about
which edit it has seen. The un-threaded channel view still re-reads for a replayed message that
arrives during a read, as it does for a live one.

**A refresh asks only for what changed** (`#203 incremental-refresh`). Once a view has been read,
the page holds that read's `next_after`, in memory only, and a refresh sends it back as `after=`;
the answer carries only what changed and is folded into the channel's store as a delta — rows it
does not mention are unchanged, never taken as deleted, removals arrive as `deleted` ids, and the
view's coverage and walk-back cursor stay as they were. Where the channel's deltas are complete (a
bridge with a change record), the poll, a live message, a live edit or deletion, pull-to-refresh,
and coming back to a page hidden for fifteen seconds or more are all deltas. Where they carry
additions only, the forty-five-second poll and pull-to-refresh stay full reads, so edits and
deletions are no slower than before — except an edit to a reply in Slack's All view, which the
server rereads only when that thread's root moves or every fifteen minutes (see "The flat view
rereads only threads that moved" in Slack's Limits) — and deltas serve live arrivals and the
return to the page. A
full read still happens on entering a view, changing channel, a stream reset, an expired or
mismatched cursor (`410 cursor_expired` or `400 cursor_mismatch`, answered at once with a full read
rather than a failure), a delta still saying `more` after four pages, and every fifteen minutes per
view. A cold start always reads in full: no cursor is saved to the device. An answer carrying `as_of`
more than two minutes old makes the freshness pill say the service is slow, with that time, instead
of claiming the list is current. An edit or removal replayed from the tail is answered only by a
newest page or a complete delta.

**A refresh that brings nothing draws nothing, and a hidden page waits** (`#226
page-energy-profile`). When a delta changes nothing — no message, deletion, read state, thread,
count or notice — and every row on screen is the one a redraw would build, the rows stay as they
are; only the freshness pill moves. A refresh nobody asked for (the poll, the read behind a live
message, coming back to the page) shows its loading line over the rows only once it has been out
for a second; the pill says "refreshing…" from the start. While the page is hidden — another app
in front, the phone locked, the window minimized — the poll does not run, a live message goes into
the channel's store with no row drawn and no read made, and an edit or removal waits the same way;
after two minutes hidden with no call the live stream is closed, which ends its keep-alive every
fifteen seconds. Coming back draws what arrived from the store, reads once what changed, polls at
once if the poll came due meanwhile, and reopens the stream from the last event it saw, so nothing
in between is skipped. Until the stream is back the pill says "Updated" rather than "Live ·
updated". During a call the stream stays open and the agent hears what arrives, as before.

**`user_message`, not `contextual_update`, and that is the substance of the feature.** A
contextual update injects text into the agent's context *without consuming a turn*: the agent
silently knows a message arrived and says nothing about it until asked. That is right for a
background note, which is what this used to be, and it cannot satisfy "read new" — the point of
having the call open is to be *told*. A `user_message` consumes a turn, so the agent answers out
loud. It deliberately does not go through the page's own `sendUserMessage`, which renders what it
sends into the transcript as the reader's words: nobody said this, and putting it under "you"
would be a false record of the conversation.

**Three modes, not a toggle, and the default speaks.** `off` is silence; `gist` has the agent say
in one sentence what arrived; `full` has it read each message out word for word. They differ only
in the **task sentence** at the head of the turn, and that sentence comes *first* — ahead of the
quoted text and its `BEGIN`/`END` fence — so that no line written by a third party is ever the most
recent instruction in the turn. Both of them end with "Then stop": this is an interruption of a
conversation already in progress, and the agent's job is to say the one thing and hand the floor
back. The default is `gist`, which is a **reversal** of the earlier off-by-default and is the
owner's explicit choice; the screen says so beside the control and in its help entry, because a
setting that spends money without being asked for on the day must be legible rather than
discovered from a bill. The storage key is unchanged on purpose — a stored `"off"` is still a mode,
so an explicit earlier refusal is not overturned by the rename, while the old `"on"` falls through
to the default it now means.

**A burst is one turn.** Arrivals are held for `RELAY_COALESCE_MS` and sent together. A coding
agent posting four lines in two seconds would otherwise take four spoken turns, one after another,
each interrupting the last and leaving no gap to answer in. Both guards below are re-checked at
the flush as well as at the door, because the call can end and the reader can press Off inside
that window.

**Two controls over one value.** The bar carries **Read new**, a button that cycles
off → gist → full → off, because the strip has room for a `.bar-button` and not for a
`.bar-select`; Settings carries the same three as a named `<select>`, where there is room to say
what they mean. Both go through one function that writes the key and redraws the other, so the two
cannot disagree about which mode is in force.

Four guards on the relay, all load-bearing:

* **A replay tail is never relayed.** The stream opens with what the server already published, up
  to 200 messages, and every attach without a `Last-Event-ID` gets all of it. Those rows belong on
  screen; announcing them says "a message was just posted" about text that may be hours old, in a
  burst, into a conversation billed by the minute — the same "existing history labelled as new"
  failure the seeding tick exists to prevent, arriving through the other door. The cost of the
  rule, stated: a message that lands while the page is between streams reaches the list but not
  the agent, exactly as one that lands while the tab is shut does.
* **Self-posted messages are never relayed.** `ops::reply` posts as the bot and the poller reads
  that post back; relaying it would have the agent hear its own answer as news and answer it, in a
  loop that bills. The ids this server posted are recorded and travel with the message as
  `self_posted`. It is deliberately not an author comparison — this server does not know its own
  bot's user id, because `HttpDiscordClient` never calls `/users/@me`.
* **The mode.** `off` is silence. The other two both speak, and both therefore cost a turn of a
  conversation the reader is already having.
* **A live socket.** There is nowhere to send it otherwise, and queuing it for the next call would
  deliver stale news at the start of a conversation about something else.

## Conversational voice providers

The server chooses a conversational provider through `[conversation]` and returns one common
`VoiceSession` from `GET /api/v1/voice-session`. The page uses the returned `provider` as its
display name and the returned `protocol` to select the wire adapter. It does not infer a provider
from the URL or use signed-URL vocabulary in provider-neutral errors.

The deployment-managed `vibe-talk-v1` WebSocket carries JSON control frames and raw 24 kHz,
16-bit little-endian mono PCM binary frames. The server first sends
`{"type":"session_started","greeting":true|false}`. A voice client then sends `audio_start` and,
when `greeting` is true, withholds microphone frames until the first `turn_complete`. Typed input
uses `{"type":"prompt","text":"…"}`. A client switching from an open microphone to typed input
sends `audio_end`, waits for `turn_complete`, and then sends the prompt; after that response's
`turn_complete`, it sends a fresh `audio_start` before resuming microphone frames. Server output is
`transcript`, `turn_complete`, `error`, or binary PCM. `quit` ends the session. The page closes the
segment only once any spoken turn already under way — the owner's words or the assistant's answer
— has completed, and both sends that `audio_end` and resumes microphone frames only a moment after
every `turn_complete`, the greeting's included; see "How a call ended".

A client may send `{"type":"interrupt"}` at any time after `session_started`. During a turn, the
server stops the response, drops that turn's remaining PCM and transcripts (including any already
queued), and answers with exactly one `{"type":"turn_complete","turn":N,"interrupted":true}`; the
next prompt on the same socket is turn N+1. When nothing is running, `interrupt` is ignored.
Either way, every accepted prompt receives exactly one `turn_complete`, so a client that waits for
it knows no later frame belongs to the interrupted turn. An `error` frame promises no following
`turn_complete`, so a client should treat it as the end of the session. A server that predates
`interrupt` answers it with an `error` frame. A client should still bound its wait for a prompt's
first output rather than trust every server to answer: read-aloud abandons a session that returns
no audio within 15 seconds of a prompt and repeats the read once on a fresh one. A turn that a
server completes with neither audible audio nor assistant text is reported to the listener as the
service not responding, so a server that cannot produce speech should send `error`, not silent PCM.

A `transcript` frame is `{"type":"transcript","role":"user"|"assistant","text":"…","turn":N,
"final":true|false}`. `final` is optional and defaults to `true`, so a provider that sends only
finished text needs no change. A provider that streams recognition should send its current
hypothesis as `final:false` as soon as it has one, rather than holding text until the turn ends:
the page shows it immediately and replaces it in place as it improves. The page keeps one row per
`(turn, role)` — a corrected hypothesis replaces the words it corrects, a fragment is appended, a
repeat of text already held changes nothing — and stores that row once, when the turn completes.
The same words in a later turn are a new row. A frame without `turn` cannot be matched and is
treated as its own turn.

### Repeatable live spoken-read acceptance

`scripts/voice-read-acceptance.py` is the provider-neutral live check for
`#47 read-aloud-overlong`. It connects directly to a configurable `vibe-talk-v1` WebSocket, opens
a fresh session per trial, sends the supplied prompt exactly, and requires a non-interrupted
`turn_complete`. The default acceptance is ten trials, no more than 15 seconds of PCM audio
between meaningful assistant transcript changes, and no more than 0.09 audio seconds per final
assistant transcript character. Streaming hypotheses are reconciled with the same cumulative,
repeat, correction, and fragment rules as the page; repeated text and user recognition do not
reset the assistant-progress clock. A promised greeting is allowed to finish before the measured
prompt is sent, and its empty audio segment is closed before the prompt boundary is measured.

The live run may consume provider time and is never part of an automated suite:

```bash
VIBE_TALK_VOICE_WEBSOCKET_URL='wss://voice.example.test/session' \
  scripts/voice-read-acceptance.py --prompt-file exact-read-prompt.txt
```

`results.json` is updated after every trial under
`debug/voice-read-acceptance/<UTC timestamp>/` by default. It retains each binary-frame and
transcript-change timestamp, PCM byte and duration counts, the maximum wall-clock span as a
diagnostic, every threshold verdict, and only lengths for the exact prompt and final transcript.
It does not retain prompt or transcript words, a prompt fingerprint, or any part of the WebSocket
URL. `--wav` additionally keeps one 24 kHz, 16-bit mono WAV per trial for ASR replay; those files
contain the spoken content and must be handled accordingly. `--out`, `--trials`, both thresholds,
and all three timeouts are explicit CLI overrides. Run
`scripts/voice-read-acceptance.py --self-test` for the
offline metric, transcript-merge, interruption, re-answer separation, privacy, and WAV controls;
it opens no socket.

The same runner has an explicit two-turn mode for `#37 read-aloud-cut-truncation`'s 0-of-2
re-answer probe. Keep prompt A, its source A, a different prompt B, and its different source B in
four UTF-8 files; none is accepted as a command-line value. Supplying all three additional file
flags selects this mode, while omitting all three preserves the ten-trial full-read mode above:

```bash
VIBE_TALK_VOICE_WEBSOCKET_URL='wss://voice.example.test/session' \
  scripts/voice-read-acceptance.py \
  --prompt-file origin-prompt.txt \
  --source-file origin-source.txt \
  --follow-up-prompt-file follow-up-prompt.txt \
  --follow-up-source-file follow-up-source.txt
```

Each of the default two trials opens a fresh socket, drains any greeting, sends prompt A, and sends
exactly one `interrupt` after receiving 2.0 accumulated seconds of PCM. It requires exactly one
interrupted completion for A, then sends prompt B on that same socket and requires the immediately
following turn to complete naturally with audible audio. The B assistant transcript stays in
memory. After the page's word normalization, ordered-word LCS coverage must be at least 0.85 for
source B, strictly below 0.15 for source A, and have a B-minus-A margin of at least 0.70. B must
also contain no contiguous eight-word source-A span that does not occur in source B.
`--interrupt-after-audio-seconds`, the three coverage bounds, the span width, and `--trials`
override those defaults; every duration override must be positive. A partial set of re-answer file
flags is rejected, and `--wav` is unavailable in this mode.

The re-answer `results.json` retains only counts, ratios, timings, turn IDs, and pass booleans. It
does not retain input or transcript text or fragments, file names, stable fingerprints, or any part
of the WebSocket URL. A green full-read report still proves only the properties it measured: the
operator must establish separately that the source message (not merely its instruction wrapper) is
about 1,000 characters. Do not treat either runner mode alone as the whole
`#47 read-aloud-overlong` acceptance.

This protocol is deliberately public and provider-neutral. Authentication, endpoint discovery,
and the implementation behind a deployment-managed socket belong in deployment configuration and
private operations documentation.

Both hosted and deployment-managed conversational providers should use the checked-in
instructions under [`prompts/`](prompts/): the scope-neutral
[`voice-agent-system.txt`](prompts/voice-agent-system.txt) followed by exactly one of three
sections: [`voice-agent-read-only.txt`](prompts/voice-agent-read-only.txt) for a read credential,
which tells the agent plainly that it can read but cannot send; or, for a credential that may post,
[`voice-agent-send.txt`](prompts/voice-agent-send.txt) while the owner has "Ask me before the voice
agent sends" on — propose, say the tool result's read-back (`To {channel}: {text}. Send it?`)
word for word, say only "One moment." on a yes, and leave the confirmation and its result to the
voice page — or
[`voice-agent-send-direct.txt`](prompts/voice-agent-send-direct.txt) with it off — post when asked,
ask no confirmation question, and say it was sent only on a `SENT` result. The prompt follows the
credential because an agent told it may send while holding a read token promises a send, has it
refused, and then invents a reason; it follows the setting because an agent given the wrong send
rules either asks a question nobody wants or announces a send that is still waiting. Keeping the
prompt beside the protocol makes changes reviewable and prevents one provider from silently
receiving a different role or chat-sending policy. `GET /api/v1/voice-agent` serves it as data:
the prompt `id`, `version`, `variant` (`read`, `write-confirm` or `write-direct`; `read` exactly
when `write_available` is false), a `fingerprint` of its text, and the assembled text itself,
beside `ask_before_send`, the chat provider's display name, the operator's time zone, the MCP path,
and every MCP tool with its `access` (`read`/`write`), whether it `requires_confirmation` (for
`post_reply`, exactly when the owner has the agent ask first), and whether it is `available` to the
credential that asked. That answer is derived from the same manifest and the same scope filter as
`tools/list`, so the two cannot disagree; a read credential is told that sending exists but is not
its to use. The text carries the owner's own words when he has saved any in Settings (`#272
voice-agent-instructions`): his edited copy in place of `voice-agent-system.txt`, then his
additional instructions under
[`voice-agent-owner-instructions.txt`](prompts/voice-agent-owner-instructions.txt), and the
capability section always last, chosen as above, so no edit decides what the agent may do. Two
booleans beside the text, `system_prompt_edited` and `additional_instructions`, say which; with both
false the text and its fingerprint are exactly the shipped ones. A bridge that attaches this server's `/mcp` endpoint to its model should fetch the
profile with the same credential it gives the MCP client. It may also load the public file at
deployment time; its endpoint, credentials, and provider-specific tool transport remain outside
this repository. Changing any of the prompt files means bumping `PROMPT_VERSION` and every pinned
fingerprint in `src/voice_agent.rs` — one per section, and one for the heading of the owner's
instructions — which a unit test enforces. A bridge that fetches the
profile once per call picks up a change to the owner's setting at its next call, with no change of
its own; **a hosted agent whose prompt and per-tool approval were pasted into its console by hand
must be given the new prompt, and the new approval for `post_reply`, every time the setting
changes** — unless its security settings let a client override its system prompt: the voice page
then hands it the assembled prompt as every call opens (`#272 voice-agent-instructions`), so only
the `post_reply` approval at the vendor is still changed by hand.

**Startup timing.** "It takes a while to start talking" is not actionable; which phase takes the
while is. For each `vibe-talk-v1` call the page records milliseconds since Start for
`session_acquired`, `microphone_ready`, `socket_open`, `provider_ready` (`session_started`),
`greeting_text`, `greeting_audio_received`, and `greeting_audible` (when the first greeting audio
is scheduled to sound), shows the ready and audible times in the connection banner, and posts the
record once to `POST /api/v1/voice-timing` after the greeting — or at hang-up, if the call ended
first. The server accepts only those field names plus `protocol` and `chat`, refuses anything
else with 400 `invalid_timing`, and logs one line such as
`voice_startup protocol=vibe-talk-v1 chat=false session_acquired=40 … greeting_audible=4050
slowest=provider_ready:2480`. Nothing said and no identifier is in it.

**When a start never reaches the voice service** (`#209 voice-socket-open`). The page gives the
voice socket 9 seconds to open. A socket that is refused, closed before it opens, or still not
open at the deadline ends the start at once. The page says "The voice service is not reachable.",
then what happened: the connection was not accepted, or nothing answered within 9 seconds. It
offers **Try again** beside that message, the large control reads "Try again", and both start the
same kind of conversation again, spoken or typed. When a deployment's own socket address names a
host other than the page's, the message says so, since that host's front door may not accept this
page's sign-in. Hanging up while the socket is still connecting is not reported as a failure. A
call that drops after it opened keeps its own wording. The startup record of a start that never
opened adds `socket_failed` (milliseconds since Start), `socket_close_code` (when the browser
reported one) and `socket_timeout` (whether the page's deadline ended it). The log line then ends
`socket_failed=460 socket_close_code=1006 socket_timeout=false`, so a refusal, a socket that hung,
and a reader who hung up are three different lines. The record is posted as soon as the start
fails. At startup the server also warns when `conversation.websocket_url` is an absolute URL on a
host other than `server.public_base_url`'s. A path such as `/ws?…` is joined to the page's own
origin and cannot go wrong that way.

**When the voice service stops answering.** A `vibe-talk-v1` call can stay open, complete every
turn, and even deliver PCM, while the listener hears nothing and sees no words. The page does not
wait for the listener to notice. It calls a turn **silent** when its `turn_complete` arrives, not
`interrupted`, with no assistant transcript text (an empty transcript or none at all) and no
audible PCM (no audio bytes, or only bytes below the threshold). Audible means some sample
reaches a magnitude of 256 (about -42 dBFS). Digital silence is 0 and speech peaks in the
thousands. Audibility is judged on arrival, before the speaker-off switch drops the audio, so
silencing the agent does not look like a dead service. A typed call plays no audio and is judged
by text alone. The page reports each of these, once per call:

| Cause | When |
|---|---|
| `silent_greeting` | the greeting turn is silent: a greeting is supposed to speak, so one is enough |
| `silent_turns` | two consecutive turns are silent. One is not enough: turn detection may close an empty turn for background noise, and a response that only calls a tool may say nothing before the answer follows. Any turn that was heard ends the run, including one the listener interrupted or one that carried an `error` |
| `no_reply` | 15 seconds after the greeting was promised, or after a typed prompt, with no audible PCM, no assistant text and no `turn_complete`. A typed call does not wait for its greeting, so there the greeting is judged as an ordinary turn and is not timed. A prompt typed while a greeting's audio or text is already arriving is held until that greeting's `turn_complete`. Meanwhile the bound restarts with each further piece of the greeting, so it measures the greeting going quiet without completing. Known limitation: a prompt sent before any of the greeting arrives, including one queued before `session_started`, cannot be told apart from it. The greeting's first text disarms that prompt's bound, and the greeting's `turn_complete` is taken as the prompt's |
| `error_frame` | the server sent `error`; the page already shows it in red and now also logs it, without its text. It is the verdict on the turn it arrives in, before or after that turn's audio: the next `turn_complete` is not counted as silent, and the no-reply bound is disarmed, so one failure is logged once. An `error` carries no turn number, so an `error` sent between turns spares the next turn instead |

The status line then turns to the `unresponsive` state with "The voice service is not responding."
That notice does not dismiss itself while the condition holds, and nothing is torn down, because
the service may come back. The next audible audio or assistant text restores `live` and says "The
voice service is responding again." Two kinds of turn count neither way. One is an interrupted
turn, unless what it cut short was half a second or more of audio with no sample at the threshold,
since a listener hearing nothing may talk over it. The other is the server's immediate `turn_complete` acknowledging
the page's own `audio_end` before a typed prompt, which is a boundary, not a reply.

Each cause is posted at most once per call to `POST /api/v1/voice-health`, and `recovered` at most
once per reported episode. The body is a closed record, for example
`{"protocol":"vibe-talk-v1","chat":false,"cause":"silent_turns","since_open_ms":21400,"turns":3,
"silent_turns":2,"audio_ms":1000,"peak":0}`. Every field is required. `audio_ms` and `peak` describe
the last silent turn, so "no audio arrived" (`audio_ms=0`) and "silent audio arrived" (`audio_ms>0`,
`peak` under 256) can be told apart without capturing a call. An unknown field, `cause` or
`protocol` is refused with 400 `invalid_health`, not echoed. A body over 512 bytes gets 413. Past 30
records a minute across all callers the route answers 429 `voice_health_throttled` and writes
nothing for them except one `voice_health_throttled max_lines=30 window_s=60` line per minute, so
one page cannot flood the log with records. (The access log still notes every request, as it does
for every route.) It logs one line of names and numbers only, WARN for a report and INFO for a
recovery:
`voice_unresponsive protocol=vibe-talk-v1 chat=false cause=silent_turns since_open_ms=21400 turns=3
silent_turns=2 audio_ms=1000 peak=0`.

What this cannot tell you:

- **Why.** Which component failed is outside the protocol.
- **Audio that is loud enough but not speech.** Noise, or speech too quiet to hear, that clears the
  threshold counts as audible.
- **The listener's own output.** The page judges samples, not speakers. A muted device or an
  unplugged headset looks healthy.
- **Whether the listener's speech reached the service.** A spoken turn the service never detects
  produces no frames at all, which looks the same as the listener staying quiet. Only the greeting
  and typed prompts are timed.
- **The hosted vendor's protocol.** It has error frames of its own. This covers `vibe-talk-v1` only.

**How a call ended** (`#268 voice-call-drops`). Calls once ended on their own while the only record
of why was the voice bridge's log, which saw the page say goodbye and could not see what made it.
The page ends a call in exactly two ways, and only these send `audio_end` and `quit`: the reader's
**Hang up**, and an `error` frame, which the protocol says ends the server's session. Everything
else that ends a call is the far side or the network, and the page says which, how far into the
call, and that the reader did not hang up — in red, kept until the reader starts a new call or
dismisses it rather than taken away after twelve seconds like other errors (a driver looks back a
minute later), and in the transcript's seam:

| Ended by | What the page says |
|---|---|
| `hang_up` | "Call ended." and nothing in red |
| `error_frame` | "The voice service reported an error 9 s into the call, and the call has ended: …" with the server's words |
| `server_closed` | "The voice service ended the call 1 min 15 s in — you did not hang up." with the reason the server gave, if any |
| `connection_lost` | "The connection to … failed 22 s into the call: it was cut without a goodbye — by the network, the phone or the voice service, not by Hang up." — a close without a closing handshake (1006), or one after a socket error |
| `suspended` | "Paused — the app was in the background.", as before, with **Resume** |

Each call that opened is recorded once, when it ends, to `POST /api/v1/voice-end`, for example
`{"protocol":"vibe-talk-v1","chat":false,"ended_by":"connection_lost","close_code":1006,
"since_open_ms":22000,"turns":2}`. `close_code` is the code the browser reported, 1000–4999, and is
absent when the page closed the socket itself; every other field is required. Anything else — a
free-text reason, an unknown `ended_by`, a code out of range — is refused with 400 `invalid_end`
and logs nothing. It has its own budget of 30 records a minute (429 `voice_end_throttled` past it,
announced by one `voice_end_throttled` line a minute), so it cannot crowd out the health record. It
logs one line of names and numbers, INFO for a hang-up and WARN for anything else:
`voice_call_ended protocol=vibe-talk-v1 chat=false ended_by=connection_lost close_code=1006
since_open_ms=22000 turns=2`. A start that never opened is the startup record's (above), not this.

A socket the page has finished with no longer speaks for any call. Its close can arrive seconds
after the page's `quit` — a server that waits out an interrupt before closing takes that long — and
it used to tear down whatever call the reader had started since. Now it only adds its close code to
the connection details; nothing else it delivers is judged or acted on.

A healthy call does not end itself. Nothing the page times — the 9-second open deadline (which only
applies to a socket that has not opened), the spoken yes's settle window, the hold below and its
cap, the microphone's hand-off, the card's countdown, the no-reply bound — closes a call or sends
`quit`; the page suite fires every one of them on a working call to hold it to that. None did before
this change either: that test is a guard, and its control makes one timer hang up to show it would
see it.

The page also stopped two things it did to the bridge mid-call. **The microphone waits across the
greeting's end** as it does across every other turn's (`VOICE_CAPTURE_RESUME_MS`): PCM a few
milliseconds after a `turn_complete` was measured to close the whole session, and only the greeting
had been exempt. And **a text turn waits for an open spoken turn**: the page's notices, a relayed
message, or a message typed into a spoken call used to close the microphone's segment at once, even
while the owner was speaking or the assistant answering. That left two `turn_complete`s in flight;
the page took the turn's for the segment's, sent its prompt into a turn still running, then took
the segment's for the prompt's and opened the microphone over the answer. Now the text waits until
anything of a spoken turn that has arrived — the owner's words, the assistant's, its audio — is
closed by that turn's `turn_complete`, and then for the hand-off after it (`VOICE_CAPTURE_RESUME_MS`,
the wait the microphone keeps), and only then sends `audio_end`; a message typed inside a hand-off
waits out the rest of it too. A turn that sends nothing for 8 seconds without completing
(`SPOKEN_TURN_HOLD_MS`) is let go, as before, and so is a turn that keeps sending without ever
completing — a trickle of hypotheses of background speech, a bridge that never sends the turn's
`turn_complete` — 30 seconds after the hold began however often its frames came
(`SPOKEN_TURN_HOLD_CAP_MS`). The connection details say which let it go.

## Signed conversation URLs

Turning on **Enable Authentication** on an ElevenLabs agent closes the public `talk-to` link. From
then on a conversation can only be started from a **signed URL**, minted from ElevenLabs' API with
an account key and good for about fifteen minutes. `GET /api/v1/signed-url` does that minting, and
`/voice` is a plain page — no build step, no CDN, no vendor bundle — that fetches one and opens the
conversation over a WebSocket.

**Failures are shown in the page, not in the console.** This route answers with a real taxonomy —
`503 elevenlabs_not_configured` names the exact setting that is unset, `502 elevenlabs_error`
carries the vendor's status and message, including things only the vendor knows, such as an API
key that lacks the `convai_write` permission — and `/voice` renders that sentence verbatim in an
alert panel. Nobody should have to open dev tools to learn a key is missing a scope. The page
redacts its own stored token before displaying anything, so an error body that echoes the request
back cannot put a credential on screen; `tests/js/voice_page.test.mjs` asserts both. Saving the
token changes the **button** — "Saving…", then "Saved ✓" — and the line beneath states what is
stored right now, because a success banner over an unchanged button leaves it ambiguous whether
the click even registered.

```jsonc
// GET /api/v1/signed-url, with the WRITE-scope bearer token
{
  "signed_url": "wss://api.elevenlabs.io/v1/convai/conversation?agent_id=…&token=…",
  "agent_id": "agent_…",
  "valid_for_seconds": 900
}
```

Four things about it are deliberate.

**It needs the WRITE scope**, even though it reads nothing. What it hands back is a working
conversation with your agent, and that agent has a credential of its own: if you gave it the write
token, then whoever holds a signed URL can ask it to post in your name. The gate on this route has
to be at least as strong as the strongest thing the conversation can do. An unauthenticated
`/signed-url` would be strictly worse than the public link that enabling authentication just
closed.

**The account API key never leaves the server.** It travels to ElevenLabs in an `xi-api-key`
*header*, never in a URL, and every error built from an ElevenLabs response body is redacted
first — an upstream is free to quote a credential back in its own error text, and that text ends up
in a log and in an API response. There is a test that asserts the key appears in no response body
on any path this route can take, including the vendor-rejection path.

**There is no unsigned fallback.** Three failures are distinguished, because they have three
different fixes:

| What happened | Status | `error` |
|---|---|---|
| `elevenlabs.api_key` or `elevenlabs.agent_id` is not set | `503` | `elevenlabs_not_configured`, naming the setting |
| read-aloud has neither `elevenlabs.voice_id` nor `elevenlabs.agent_id` | `503` | `elevenlabs_not_configured`, naming both — either one fixes it |
| the message to be read aloud has no text | `422` | `nothing_to_read`; no vendor call is made, so nothing is billed |
| ElevenLabs refused us, or is unreachable | `502` | `elevenlabs_error`, carrying the vendor status |
| The caller has no token, or only the read token | `401` / `403` | `unauthenticated` / `forbidden` |

**The answer is `no-store`.** The minted URL is itself a bearer credential for the next fifteen
minutes.

The `/voice` page expects the agent's output audio format to be **PCM** (`pcm_16000` is the
default). It reads the format out of the conversation's initiation metadata and says so plainly if
it is something it cannot decode, rather than playing noise.

### The status line is a message, not a fixture

It used to be a permanent grid row at the top of the dock: a strip of a phone screen held on every
frame, for a line that is blank most of the time. The governing rule of the layout is that the
majority of the space belongs to content, and a row reserved for something that is usually not
there is the clearest possible violation of it.

It now ships hidden, appears when there is something to say, and takes itself away after six
seconds — or immediately, if you tap its dismiss control. **Dismissal is a hide, not an erase**:
what was last said stays readable to anything that asks.

**It is an overlay, and that is the one exception to this page's "nothing is positioned" rule.** A
row that appears and disappears resizes `#scroll-area`, and resizing the scrolling element moves
the transcript under the reader's thumb — the exact defect `#47 scrollback-stability` exists to
remove. Floating it costs no reflow. The CSS says so, at length, so it does not get "cleaned up".

**A message that goes away can hide something nobody saw**, so nothing that must survive is kept
only here: a failure is in the `#error` panel until it is fixed, a close code is in the connection
detail on the settings screen, a conversation boundary is a seam in the transcript, and live /
muted / idle is carried by the controls themselves. What is left on the strip is a thing that was
true a moment ago.

Two things moved out of it rather than away:

- **The channel's own summary** — how much is loaded, and whether that is all of it — is now an
  entry at the head of the channel view, in the same seam idiom the transcript uses for a
  conversation boundary. It is a standing fact about what you are looking at, and it scrolls off as
  you read rather than expiring after six seconds.
- **The "paste your token" instruction** is in the sign-in screen's own body. Firing a toast at
  page load to say it was a message over the screen that was already asking.

`02-idle` is the screenshot that proves the issue is fixed: the idle phone, at rest, with nothing
holding a strip. `08-clear-armed` is its positive control — the same page with something to say,
saying it — because a page that had simply deleted the line would satisfy the first alone.

### The messages you have not dealt with yet

`#50 todo-view`. A long backlog of assistant messages is a to-do list in practice: some want a
reply, some only want reading, and nothing on this page could tell them apart. **To do** is a
sub-toggle of the channel view — the same list filtered, not a third tab — and it reads a
different route rather than filtering the rows already on screen, so there is one definition of
"dealt with" and it lives on the server.

**Done/archive is OURS, and this is the screen that has to say so.** `#61 unread-status` settled the
direct Discord case: a bot has no ack to send and no read-state field to read. Marking something
dealt with here does **not** mark it read in the source service, and reading it there does **not**
clear it here. A deployment whose provider bridge explicitly supports upstream read marks gets a
separate **Mark read in** *the service* action, under each message's **⋯** menu; it moves that
service's own read marker and never changes Done/archive state.

**The store is single-tenant.** Every mark is yours, with no column saying whose. Sharing
one deployment between two people would silently merge their inboxes; that is not a
configuration this decision survives, and it would have to be revisited rather than worked around.

Three acts, each reachable from a control:

- **Done**, on the row. One message, and the answer names it, which is what makes the undo exact.
- **Clear the backlog**, above the list. Bulk and destructive, so it says how many it is about to
  take, takes two taps — the same armed idiom the dock's Clear control uses — and lapses back to
  safe after a few seconds. It sends `{through: <newest id>, limit: <the window it read with>}`
  and the **boundary is included**: the row you gave up on is part of what you gave up on, and
  that is tested at the boundary. The `limit` is not decoration: the server resolves `through`
  against a window of its own, so a boundary sent without one is resolved against the server's
  default and clears messages that were never displayed. Clearing work you never saw is the
  worst failure this view has, so the window the list was READ with rides along with the act.
- **Undo**, as a chip, carrying the exact set the server said it cleared. Not "the last N", which
  is a different set by the time it is pressed. A bulk clear that also claimed messages you had
  dealt with *earlier* would resurrect them on undo, so the server reports only what it really
  changed.

**The channel does not stop while you work through it.** A message can arrive on the live stream
or come in on the 45-second poll while the filter is on, and it is by definition not dealt with —
so it is one more thing to do, and everything that *describes* the list moves with it in the same
moment the row appears: the head of the list, the count on **Clear the backlog**, and the Newest
chip when the row landed off screen. A view that drew the row while its own head still said
"nothing left to deal with" would be worse than one that missed it — a false statement with the
counter-example visible directly beneath it — and a bulk control that undercounted would clear
more than it offered to.

Two consequences worth stating. The walk back is **not** offered over a filtered list: the cursor
belongs to the unfiltered channel, and stepping back with it would prepend messages you have
already dealt with above ones you have not. And the head of the list says "**3 of 4**" rather than
"3 messages", for the same reason `#62 message-count-accuracy` exists — the second reads as the
size of the channel.

**What is not here yet**, said plainly rather than left to be discovered:

- **The gesture layer.** `#50` asks for a swipe to dismiss and a press-and-hold to declare
  bankruptcy. Disambiguating horizontal from vertical intent on the one list this page scrolls is
  a change of its own; the acts land first, each reachable by a control a keyboard can also get
  to, so a gesture becomes a second way in rather than the only way.
- **Detecting a reply.** `#50` also wants a message to leave the list when it is answered through
  Discord's reply affordance. That is *derived* state and it needs a reply reference on the
  server's `Message`, which is a wire-format change touching every struct literal that builds one.
  Until it lands, "dealt with" here is always **declared** — which is also why nothing yet has to
  decide what happens when derived and declared disagree.

### Show read, Collapse read, Hide read

`#221 read-modes`. The button beside Pace names what the list does with messages already read —
archived (Done), your own words, and placeholders read automatically — and each tap moves it on.
Each mode has its own icon and is named in full beside it — on a desk's one-row dock in two short
lines, so the row stays one row — and the tooltip says what the mode does and what a tap moves to:

- **Show read** (the default): every message, the read ones greyed.
- **Collapse read**: each run of two or more read rows becomes one line, `… 12 read messages …`,
  counting messages rather than rows, and a tap on the line opens that run. A thread's first
  message and the newest message in the list stay in view however read they are: the first says
  what the thread is about, the newest is where the conversation stands. A run grows, shrinks or
  splits as messages are dealt with or put back, here or elsewhere, without moving the message you
  are reading; a search or Links opens every run while it is choosing rows; the Pinned list is never
  collapsed, nor are replies you have gathered under their message; and a jump to a message inside
  a run opens it.
- **Hide read**: read messages are left out, the queue it always was.

What counts as read is one rule for every mode, so Collapse read collapses exactly what Hide read
hides. On Main, a message whose thread holds unread replies, or new ones the page has not loaded, is
kept by both, as though it were unread itself (`#222 unread-replies`; see "A message's replies can be gathered under it" above). The mode is kept with the rest of the UI state; a record written before the modes, which
says only whether Hide read was on, is read as Hide read or Show read.

`make -C vibe-talk read-modes-browser` enters a thread with a 45-message read run in each mode in a
real Chromium at 412x915 and 1280x800, against a loopback fake API, and checks where the first unread
message lands and that the run's line is a 44px target a real tap opens. It then picks Main under Hide
read, where the read question is kept by its four unread replies and says **4 unread** beside **52
replies**, on one line and in the accent, and a real tap on that count opens the thread at the first
of them. Pass `SCREENSHOTS=/tmp/shots` to keep a PNG of each step.

### A folded message says how much it is hiding

`#244 elision-word-count`. A long message arrives folded to its first three lines, and those lines
alone do not say whether it is barely over the fold or a wall. So the line under them says how many
words the fold keeps back: `(~240 more words)`. Opening the message takes the line away; folding it
again brings it back. It is under the message text in the transcript and in the channel, on a row
combined from several messages, on the reply screen's earlier messages (drawn as the list drew them,
`#247 reply-continuity`, so none is a line shorter there than on the list), and on a body the server
rendered, where the words counted are the ones on the screen (a link counts as its text, never as its
address).

The number is a **floor**, and the `~` says it is an estimate. The fold is a three-line CSS clamp, so
where the cut falls depends on the row's width, the text size and the browser's line breaking; asking
the layout engine row by row would put a forced layout per long message back into every redraw. So
the page works the count out once, when the row is built, from the text alone — for **every width a
line can be**, twenty classes from a narrow phone at the largest text size to a wide desk — and the
line under the text, a CSS size container as wide as the text and at its size, shows the label for the
class the row is drawn in: a phone, a desk's column, one of two columns, a reply's inset, the reader's
text size. Each count is taken at the widest line of its class with every character at the narrow end
of the common interface fonts, so it can say fewer words than are hidden and never more; under ten it
says nothing at all. Rounded down: exact under twenty, then to five, ten and a hundred. The first
version used a phone's line everywhere, and on a desk said words were hidden under a message three
desk lines held whole.

**Text poor in spaces is counted in characters.** Chinese or Japanese has no spaces to count words
by, and a long bare address is one word, so a clamp hiding most of either would have hidden "no
words". Where what is hidden runs to more than twelve characters a word, the line says
`(~400 more characters)` instead (and nothing under sixty). On a row with a rich header (`#242
rich-headers`) the words the header restates are counted out while it shows them, and back in with the
switch off.

**Not everything in a message is prose, and the count follows what the clamp does with each block.**
A **table** scrolls sideways in a box of its own, and the clamp counts no line inside such a box: it
draws the table whole and goes on counting lines under it. So a table counts as shown, every word of
it, when it starts inside the three lines, and as hidden when it starts below them; a message that is
only a table says nothing. (Counted a line per cell, the first version said `(~95 more words)` under a
table with every word on the screen.) A **code block** is counted in lines of its own — blank ones
too — in code's monospaced face at its smaller size, and where the clamp's last line is one of them the
block's padding shows the top of its next line, which counts as shown. A **list** is a line per item,
however short. A message holding anything the count has no rule for gets **no label at all**, rather
than one worked out from a guess; every element the server's sanitizer sends has a rule, and the page
suite holds the two lists together. (An image never reaches the count: the server sends it as a link.)

`tests/elision_browser.py` (DAG step `elision-browser`) holds every label to what the clamp really
hides, found by the rects of the characters in Chromium, in seven layouts: phones at 360 and 412 pixels,
412 at 80% and 150% text, a 1280 desk, the widest reading column, and two columns — prose, Chinese, an
address, a rich header's row, a table alone, a table over prose, a code block and a list of short
items.

### A long message can be read as a summary of itself

`#49 cached-summaries` landed as a **server half with no caller**: the endpoint answered, the store
cached, the policy-versioned key invalidated correctly, a startup sweep collected what a changed
policy orphaned — and nothing on any screen ever asked for a summary. A cache nobody spends is a
cost with no benefit.

**Summaries** is a chip over the channel list, beside "Collapse all", and it is a **mode** rather
than a per-row control. Collapsing to a prefix stays the default and costs nothing; a reader who
turns the mode on sees each folded row's opening three lines replaced by one line about the
message, with the message itself still one tap away behind **More**.

Four properties, all of them about cost rather than about appearance, and all of them tested:

- **"Long enough to summarise" is `COLLAPSE_OVER_CHARS`** — the same sentence `#47
  scrollback-stability` folds by, in the same function. A short message has no fold control and is
  never sent anywhere. A second threshold here would be a second definition of short, and the two
  would drift.
- **One request per message, ever.** The record is written BEFORE the await, not in the response
  handler, so a hundred scroll events over one row cost one request even while the first answer is
  still in flight — which is the case that matters, because a phone on a slow connection is where
  the events pile up. That is tested against a fixture whose answers are HELD, not merely against
  one that replies instantly: with an immediate reply, a record written on completion looks
  exactly like a record written before the request. The forty-five-second background poll, which
  replaces every row on screen, also buys nothing a second time.
- **Only what you are looking at.** Rows are asked about as they come near the viewport, so
  nothing is spent on messages nobody scrolls to. And when the server answers that it has **no
  summariser configured at all**, the page stops asking entirely: that is a fact about the
  deployment, not about the row, and one request settles it for the whole view rather than one
  per row — each of which carries a Discord window fetch behind it.
- **The page names the summariser it actually got**, quoting the server's own answer at the head
  of the view. Showing a model's paraphrase of somebody else's message without saying where it
  came from would be claiming a reading the reader cannot check. `web/voice.js` contains no
  summariser name of its own, which is asserted, because a constant here would outlive the
  deployment it described.

Two answers are deliberately different. `below_threshold` is the **server's** own, stricter
threshold saying the message is short enough to read as it is: settled, never asked again, the row
keeps its opening lines, and it is **not** drawn as a failure. Anything else without usable text
is a **failure**, and a failure is now visible: the row keeps the message — that is the one thing
the reader still has — and is tinted **red**, with the words `summary failed` where the summary
would have been and the reason on the row itself. The words matter as much as the colour, because
a colour is not reachable by a screen reader.

Red means "a summary was attempted for this row and it failed", whatever the reason — including a
server with no ElevenLabs credentials, which is a failure the reader is entitled to see rather
than a quiet sentence they will scroll past. Rows that were never asked about stay plain. Fifty
failures produce **one** status message with a count, not fifty; the sticky error panel is not
raised for a summary failure at all, because taking the channel away is a worse outcome than a red
row; and the deployment-level reason is stated once above the list, where a permanent fact belongs
rather than in a strip that fades after six seconds. Leaving the mode and re-entering it retries
an ordinary failure, because a failure is not a verdict — but it does not re-fire one doomed
request per row at a server that has already said it has no summariser.

Entering the mode changes the height of every row on screen at once and adds a sentence above the
list, which is the mutation browser scroll anchoring does not cover. It goes through the anchor
helper `#47` built, and the page suite proves it with a negative control built from a copy of
`web/voice.js` with that call removed.

### The channel view walks back, one server-cursored step at a time

`GET /api/v1/channels/{id}/page` landed with **#53 stepped-retrieval** and had **no web caller at
all**: the page read `/messages`, which is a window, so the oldest message on screen was simply the
end of what this interface could ever show — with nothing on the screen saying so.

The channel read now uses the cursored route. Reaching the top of the list takes the next step by
itself, which is the gesture people already have — except while a search, Links or whose messages is
on, where the button is the only way back (see [Searching further back](#searching-further-back));
**Older messages**, above the list, is the same
action as a control — it is what *says* more exists, what a keyboard can reach, and what reports a
step in flight. It is absent once the walk has reached the beginning, following the same rule Hang
up does.

Older messages arrive **above** the viewport, which is exactly the mutation a browser's own scroll
anchoring does not cover — the same case as collapsing a message the reader has already scrolled
past. It therefore goes through the anchor helper `#47 scrollback-stability` built, not a second
mechanism beside it.

Two things this makes possible that were not:

- **A re-read no longer discards the walk.** The newest page replaces only what it covers; rows
  older than it survive when the server says `has_more`, and are dropped when it says the page *is*
  the whole channel. Without that, a background poll would delete the history out from under
  somebody reading it, every forty-five seconds.
- **The count can finally be true.** `#62 message-count-accuracy` says a number is the channel's
  own only when the server reports the set is complete. Walking back to the beginning is the first
  way this page has ever been able to reach that state, and the summary then says
  "13 messages from lead team" instead of "the most recent messages".

Snowflakes are compared as **strings**, length first. A Discord id is nineteen digits, `Number()`
rounds it, and two adjacent ids come out as the same float — an ordering test written with `<` on
numbers quietly answers false for a list that is perfectly ordered.

The throwaway server the screenshot harness starts sets `discord.max_fetch_limit = 8` on purpose:
`--fake-discord` seeds about a dozen messages, so at the default ceiling the channel arrives in one
read and none of this is exercised by any picture.

### The transcript walks back the same way the channel does

`#48 transcript-storage` made the record of a call **durable**, and `#128 transcript-history` is
what finally lets you read it. Before it, reopening the page restored the last conversation and
stopped there: everything older was on the server, in SQLite, and unreachable from the only
interface that has it.

The restore is now a **bounded suffix of the whole record**, oldest-first within each call, and
**Earlier turns** above the list takes the next step — the same control, the same automatic step
on reaching the top, and the same anchored prepend as the channel. There was no reason for two
mechanisms, and two would have drifted.

The cursor is a **row value** over `(at_ms, conversation_id, seq)`, not a timestamp. Two calls can
have turns at the same millisecond — a restored one and a live one, or two devices — and a cursor
that compares only the clock either loses a turn at the boundary or repeats one. The server asks
for one more row than it means to return, which is how `has_more` is answered without a second
count query.

Where a call ends and the next begins is drawn as a **seam**, because an unbroken list of turns is
itself a claim that it is all one conversation, and across a walk back through weeks of calls that
claim is false more often than it is true.

### How message text is drawn

`#217 markdown-blocks`. A message is drawn the way the chat service's own client draws it: a `- `
list has round bullets with each item's text hanging clear of them, a continuation line stays
inside its item, a blank line is a gap between paragraphs, a heading is a little larger than the
text, a table is a table, and code is set apart. The page used to render Markdown itself, one block
per line, and drew lists as literal dashes and ran paragraphs together; it renders nothing now.

**The server renders every body**, on the routes the page reads — the timeline, the to-do list, the
live stream, a post's answer and the pins — and sends it beside the text as `content_html`:

1. The provider's own syntax is turned into Markdown first. Every provider's mention, `<@id>` or
   `<#id>`, becomes a chip showing the id; a `<https://…|label>` link from Google Chat or Slack
   becomes a link named by its label — or, when the label is the address itself, a link whose
   text is its address — except inside code, where it is shown as written.
2. [comrak](https://github.com/kivikakk/comrak) parses GitHub-flavoured Markdown — tables,
   strikethrough, task lists, and bare `https://…` and `www.…` addresses as links — with chat's two
   line rules: a single newline is a line break, and only a blank line starts a paragraph. A line
   of `---` under a sentence is a rule, not a heading.
3. On Google Chat and Slack, where `*a*` means bold, an asterisk emphasis is drawn bold; `_a_` stays
   italic and `**a**` is bold everywhere. Neither service has a double-underscore bold, so there
   `__init__.py` is the file it names; on Discord it reads as CommonMark reads it, with "init" in
   bold, and backticks keep it literal anywhere. That is the provider's `markup` setting
   ([Configuration](#configuration)): a Slack provider is always Slack's, and a Discord-protocol
   bridge says `markup = "google-chat"` when it serves Google Chat.
4. **A list is nested the way its writer indented it** (`#237 nested-lists`). CommonMark nests a
   line only when it is indented as far as the item's text — three columns under `1. `, four under
   `10. ` — while chat indents a level by two spaces, one, or a tab, whatever the marker. A list, an
   item, a fenced code block or a quote written further in than the item above it is drawn under
   that item, with the numbers it was written with; a numbered item that counts on from the one
   above it, ` 9.` under `8.`, stays beside it. Two shapes are drawn as the service draws them. On
   Google Chat and Slack, a line of prose at the left margin straight after an item starts a new
   paragraph, where CommonMark would run it into the item — when it reads as a line of its own. A
   line that starts in lower case, that a code span, emphasis or a link runs on to, or that follows
   a line of 60 characters or more stopping mid-sentence is the rest of an item an editor wrapped,
   and stays in it, unless it ends in a colon; and no line is set apart where that would change any
   other block, such as the next `2.` of a numbered list, or any character the message shows, such
   as a pair of backticks. On Google Chat, which has no numbered
   lists — `1. Goal` is a line of text there, and the bullets after it a list indented beneath it —
   bullets at the margin on the line straight after a numbered item are that item's, and so are a
   heading line after them and the bullets under it, `In review:`, so every bullet of such an
   outline is drawn at one indent. A marker CommonMark has already read as part of an item's text,
   such as `    1. c` four columns in under `  1. b`, stays that text.
5. An image is drawn as a link to it, so no phone fetches it from wherever the message pointed. A
   link to anything but a well-formed `http` or `https` address is drawn as its text with the
   address after it, so the reader still sees what the message pointed at.
6. A link whose text is its own address — a bare address, `<https://…>`, or a chat service's
   address labelled with itself — is drawn with a **short label** when it is a GitHub page that has
   one (`#227 github-link-abbrev`): `widgets#123` for a pull request, an issue or a discussion,
   `widgets#123 (comment)` for a comment in one, `widgets@4f21ab0` for a commit,
   `widgets@v1.0...v1.1` for a comparison, `widgets@v1.2.0` for a release, `widgets run 123456`
   for a run of Actions and `widgets:render.rs#L10` for lines of a file. The repository's owner is
   left out. The link still goes to the same address, and a pointer resting on the label shows it.
   A link its writer named keeps its name, code is never relabelled, and any other address — other
   hosts, and GitHub pages with no short form — is drawn as written. So is any address a browser
   would open somewhere other than its label says: one whose path holds a `.` or `..` segment
   (spelled with `%2e` or not), a backslash or a control character, and one under a word of
   GitHub's own that no account may take, such as an organisation's `/orgs/NAME/discussions/3`.
   Copy text still has the address as it was written. A voice says the same page in words —
   *"widgets pull request 123"* — only where the page shows a label, and any other address by its
   host, *"a link on example.com"* (`#187 speakable-ids-urls`). The rules are a table in
   `src/link_labels.rs`, where another host can be added.
7. Raw HTML written in a message is shown as the characters it is, and
   [ammonia](https://github.com/rust-ammonia/ammonia) then keeps only a short allowlist of
   elements and attributes; see [Security](#security).

**A plain message carries no HTML.** When drawing the text as paragraphs would look the same —
most chat — `content_html` is left off and the page draws `content` itself, so a window of messages
is not sent twice.

**What one body may cost is bounded before it is rendered**, because anyone in the channel writes
the text and it is rendered on every page load. A body past a bound is drawn as plain paragraphs —
the same as a sentence with no Markdown — never as an error and never as unsanitized text:

| Bound | Value | What it stops |
|---|---|---|
| Length | 16 KiB | Everything below is linear in the text, so this holds one body to a few milliseconds. Four times the longest Discord or Google Chat message; a Slack post near its own 40,000-character ceiling is drawn as text. |
| Table cells | 2,000 | GFM pads every row to its header's width, so a wide header over many one-character rows is C × R cells: 2,000 characters made 840 KB of HTML. Counted from the text before parsing; past the budget the message renders with tables off. |
| Depth | 32 | The sanitizer's HTML parser slows with every element still open, so thousands of nested `>` took seconds. |
| Growth | 8 × the text + 8 KiB | HTML far larger than its words, such as hundreds of empty checkboxes, is not sent or kept in the offline copy. |

A message saved on the device before the field existed is drawn the same way: as its text, never as
markup. Nothing a model reads carries the field: `/messages`, `/digest`, one message by id, the MCP
tools, and `/page` unless the page asks with `render=html`.

**What reads which form.** What a row draws is what the fold measures, what search matches and
what names a thread by its first message. **Copy text copies what was written**, Markdown and all,
because a pasted message should keep its own markup (`#198 copy-message-text`); pinning sends that
text too, and the device voice and the live relay still say the server's spoken form, falling back
to it — which is why `content` still travels beside `content_html`.

The page, its script and its JSON travel **compressed** — brotli or gzip, as the browser offers —
and the live stream and read-aloud audio do not: an event has to reach the page when it happens,
not when a compressor's buffer fills, and audio is compressed already.

### Rich headers

`#242 rich-headers`. The server looks at the first line of every message it renders for the page
(`src/heading.rs`) for a heading an agent wrote, and sends what it finds as `heading`: `tag`, `time`
and `title`, each optional and each plain text, and `cut`, how much of `content` they restate, in
UTF-16 units. A message it is not sure of has no `heading`, and is drawn as it always was.

- **A tag is required**: a leading `[tag]` of one to four words, or a one-word `(tag)` — leading, or,
  from a bot account only, trailing a title as in `Release notes (ops2):` — that the same author has
  opened at least two messages in the channel with. A paren tag needs that second message because a
  parenthesis opens plenty of ordinary sentences; the server remembers recent paren tags per channel
  and author, in memory and bounded, from every read, the live stream and the pins it serves. A
  link, a checkbox, a footnote, `[1/3]`, `(edited)`, `(cont.)` or an aside such as `(thanks)` is
  never a tag.
- **The title** is the rest of the first sentence, up to `:`, `.`, `?` or `!`, with emphasis around
  the whole of it unwrapped. It is left out when it is over 80 characters, holds any other markup
  (an emoji shortcode such as `:tada:` included), or ends mid-line in a full stop with no time, as a
  reply's `Yes. The run…` does.
- **The time** is one clock, `10:52 PM ET`, `15:05Z`, kept as written, and only from just after the
  tag, the end of the sentence, or parentheses inside it. A time anywhere else means no time and no
  title; another parenthesised aside stays in the title, times and all.

**The body is not cut.** The restated words stay in it, marked: in a rendered body the server wraps
them in `<span class="rh-src">` (or gives the class to a first paragraph they fill), only when the
body provably begins with their own rendering, and the sanitizer keeps that one class; a plain body
stays empty and the page marks `content[..cut]` itself. The page hides the marked words while it
shows the heading, so turning **Rich headers** off draws every row exactly as before, and nothing
that reads the text — Copy text, speech, search, the Links filter — sees a difference.

**Where the heading goes is decided by the stylesheet.** Each headed row carries a length class from
its strings alone, and each list is a CSS size container: a class joins the header line only where
the list is wide enough for it beside the row's buttons, and is otherwise the row's first line. No
layout is read, and nothing is cut short with an ellipsis. Like `content_html`, `heading` is sent
only on the routes the page reads.

### Finding something in what is on screen

A magnifying glass floating over the top-right corner of the list opens a field, and the field
filters **the messages already loaded** — the channel and the voice transcript alike, because they
are one switch apart and a filter that came off when you looked at the other list would be worse
than none.

The glass **costs no row** (`#197 floating-search`). It sits on the same line as the freshness pill
("Live · updated 07:23"), so the header stays off the main screen whenever it has nothing else to
carry. It was drawn the pill's height until the owner called it very small (`#211 link-filter`): it
is a 36px disc now, hit at 48px, and the bar it opens is 48px tall with a 17px field. Tapped, it
grows along that line into a bar across the top of the list — field, count, the Links and Pinned
filters, and the glass again to close it — which is also the one place an on-screen keyboard never
covers. With both filters on a 360px phone the count takes two short lines, "2 of 13" over
"loaded", so the field keeps 140px. The glass and the filters keep those sizes under a larger
system font, the way a phone's own buttons do: the text on the bar grows, and the buttons do not
take the field's room for it, so at 150% type the field still shows what is typed. The pill steps
aside while the bar is open, and the list makes room for the bar at its head so the first match is
never underneath it. A reader who moved the control bar to the top keeps it there while searching:
the field no longer shares that row.

**It matches what a row says**, as the reader sees it, and the author's name: a word split by
emphasis marks is one word, a mention is found by the `@id` it is drawn as, and the Markdown source
is not searched. The address of every link a row draws is searched too, so a pasted address still
finds its row (`#217 markdown-blocks`).

**Every term has to match**, and matching is substring rather than whole-word: a second word is
typed to narrow, and "runner" has to find "runners". **Double quotes group words into one term**,
so `"mac runner"` finds the phrase and not the two words scattered through a paragraph; an
unterminated quote groups to the end of what has been typed, which is the state the field is in
for every keystroke between the two quotes.

The count beside the field says **"2 of 13 loaded"**, and the last word is load-bearing. Since the
section above, what is on screen is a bounded suffix of a longer record, so "no matches" and
"nobody ever said that" are different answers and a bare number would let you conclude the second.
For the same reason, a search that matches nothing says so **where the messages were** rather than
only in the corner: every row is still in the list, hidden by a class of its own, so neither pane's
empty state fires and the reader would otherwise get a blank screen. That sentence stands at the
head of the list, and typing — or turning Links or a kind of link off, below — that empties the
list takes the reader there, clear of the bar: the channel's composer still runs below an empty
list, and a reader left at its foot had the sentence above the screen. Text that matches again, or
a kind turned back on, puts back the message the reader was on before the list emptied (see the
next section), and no jump to the newest message is offered over a list with nothing in it (`#212
link-filter`).

A class of its own, and not the `hidden` attribute, because `hidden` is where this page already
records the *other* reasons a row is off screen — To do mode, dismissed, which view is up. A filter
that borrowed it would put archived rows back on screen when the search was cleared.

Rules the page drew itself — the seam between two calls, the date rules — **go away with the rows
they were explaining**. A seam says the agent below it never heard the words above it, and left
standing over a filtered list it says that about two rows that are no longer adjacent.

### Searching further back

`#262 search-load-older`. The owner: *"searching for a term XYZ in two column view found zero hits; in
one column, scrolling up while searching loaded older pages and found 3 ... 2 column view should not
PRECLUDE loading hits."* The search reads only what is loaded, and an older page was read only by
scrolling to the top of the list, which nothing bounded under a search. Measured in Chromium: once a
search had emptied the list, each older page was drawn and then hidden by the search, which moved the
scroll position at the head of the list enough to ask for the next one, and the page read the
channel's whole history back by itself — on Main, which holds no thread replies, a walk to the start
that finds nothing. That is the scrolling that found the owner's 3 in one column. In two columns the
left column is Main, so it found nothing however far back it went. The button at the head of the list
said "Older messages" and nothing about finding.

So **while a search, Links or whose messages is on, scrolling to the top of a list loads nothing by
itself**: the button at its head, or Enter in the search field, is the way further back, and it is
bounded. Close the search, or clear what it filters by, and scrolling to the top loads older messages
as it always did.

While one of those filters is on, each column's button at the head of its list says **"Search before
Oct 3"** — the day that column reaches back to ("Search replies before …" in a thread) — and
**Enter** in the search field, a phone keyboard's Search key, does the same as a press. One press reads
up to **five** older pages, starts no page that the last one's time says would end past **fifteen
seconds**, and stops at the end of the page that brings **three** new matches. Meanwhile the button
says **"Searching… 2 of 5 · Stop"**, and pressing it aborts the read on the wire. Every press is one
more search and nothing chains pages by itself; the button always says how far back the next one
starts. Any change to the search, closing the right column's thread, leaving two columns, another
channel or view, a reply's jump or a filter's own walk back ends a walk where it is, and only one runs
at a time. Main drawn from All's rows reads its own newest page first, for the cursor the walk goes on
from; that read is not counted as one of the five.

What a walk found is counted among messages **older than where it started**, so a message arriving
live or a poll can neither stop it nor be reported as found, and in messages, as the count is: a
combined row is every message it holds, so the count beside the field and the lines between matches
add up. It is said once, under the button, until the search changes: *Found 3 in 2 pages (98
messages), back to Sep 30.* The button holds still while the walk runs and the matches open directly
under it, the line above the first saying how many unmatched messages it skips; scroll away while it
reads and the message you scrolled to is kept where it is instead. With nothing matched,
the sentence where the messages were says how far back that is: *Nothing in the 643 messages loaded so
far, back to Sep 28, matches that search.*

**Hide read steps aside while a search chooses rows**, on a chat service with threads, as Collapse read
does: an old match is read by definition, and a walk under Hide read would read pages and draw almost
nothing of them. Read matches show greyed, the read mode kept on the device is untouched, and closing
the search brings Hide read back. On a chat service without threads Hide read is a list of its own,
read for itself; there it stays in force, no walk is offered, and the sentence says how to search older
messages instead.

**Main holds a thread's first message and none of its replies**, and that is most of why a search on
the left of two columns found nothing. Main's sentence says so; in one column a search nothing on Main
matches offers **"Search All — thread replies too"**, which opens All with the search kept, and goes
the moment two columns come back (their left column is Main by the layout); in two
columns the cards on the right are found by what the thread's first message says (where the card shows
its summary) and by the replies the page has loaded, and the thread selected there walks its own older
replies. A walk reads only the routes a scroll up reads: no server route is new. Finding a reply the
page has never loaded, in two columns, needs a server search, which waits on the persistent cache.

### Nothing moves the list but the reader

`#280 scroll-stability`. The owner: *"I swear I just saw an instance where it jumped me to the bottom of
the screen in the main message view (two column mode)... I don't think I touched anything. I think it
was something updating underneath."* It was a read: every refresh took the reader's place as it went
out and settled to it when it landed, seconds later behind a bridge, so a reader on the newest line who
scrolled up while it was out was thrown back to the bottom. The rules, in both columns:

- **A read settles to where the reader is when it lands**, never where they were when it was sent: a
  reader up the list keeps the row they are on, within a pixel, and only a reader still on the newest
  line follows it. That holds for the first read behind the rows this device saved, too.
- **A message arriving live is followed only by a reader already on the newest line**, asked before it
  goes in; everyone else is left exactly where they are and offered the jump to it.
- **In the right column, a reader on the newest line stays on it** when what is in the list changes size
  under them with nothing scrolled — above all the card under the pointer drawn whole — and a reader up
  the list is held by the card or row they are on. Main keeps `#207 scrollback-jump`'s rule: a newest
  message growing under its reader with nothing redrawn offers the jump to its end.
- **A thread opens at its foot** when nothing in it is unread, or only its newest message is (unless
  that message is taller than the screen, when its head shows); one with older unread messages still
  opens on the first of them (`#221 read-modes`). The list of threads opens at its foot and stays there.
- **Moving the focus never scrolls a list** unless what takes it is off screen, and then the least that
  shows it. ] and [, j and k, and Tab glide nothing in the other column.
- **The other column glides to a selection's counterpart (`#261 two-column-connectors`) only when the
  mouse rests on it.** The selection outlives the pointer: a mouse crossing the cards on its way to the
  dock or the top bar leaves the last card it passed selected, and Main used to slide thousands of
  pixels to that card's root 1.5 s after the pointer had gone — the owner's jump in two columns with
  nothing touched. The wait starts when a real mouse move lands on the selection, ends when the mouse
  moves on from it or leaves the window, starts again if it comes back, and glides only if the mouse
  is still there: on it, or within 8px of where it last moved on it, which a card drawn whole moving
  out from under a still pointer does not count as leaving. A selection a redraw hands on never glides.
  While a card is highlighted (`#277 thread-card-focus`), the glide follows it instead of the
  selection: the click that highlighted it, or the mouse coming to rest on it, starts the wait for its
  root, and the mouse over any other card, or over Main, starts none. Enter highlighting a card glides
  nothing, as no key does.

`tests/scroll_stability_browser.py` (DAG step `scroll-stability-browser`) measures them in Chromium at
1280x800 in two columns and 412x915 in one: the first row on screen and its offset through live
messages, a refresh, the read sent from the newest line and landing after a scroll, summaries and focus
moves, and at the desk, sampled at every frame for 2.5 s, the mouse crossing the cards to the dock and to
the top bar, ] on its own, and the mouse going on from a card it highlighted over another card to the
dock; a reader on the newest line following; the threads and their list opening at the foot.

### A filter keeps your place

`#243 filter-keeps-selection`. The owner: *"when you adjust the filter, the highlighted message MUST
stay on the screen. Ideally it stays on the screen in the same Y location without a 'jump.'"* His case
was finding a pin: search, **Pinned**, select the message — and as soon as the filter was cleared, the
list jumped and the message he had just found was gone.

So every change of what the list shows keeps the reader's message where it is on the screen: the
search text typed or cleared, the search closed (Escape or the glass), **Pinned** and **Links** on or
off, **PRs**, **Commits** and **Actions**, and **Show read**, **Collapse read** and **Hide read**. The
message kept is the **selected** one while it is on screen — chosen by the keys, the mouse, or
silently by a tap — and otherwise the one at the top of the screen, or the newest line for a reader
on it. Afterwards it stands at the same height, as near as the ends of the list allow:

- a pin that is the second part of a post the channel combined into one row stands where
  it stood in the Pinned list, and the part above it goes above the screen's line, not below it;
- under **Collapse read**, a message folded into a run is kept by the run's line;
- a message the new filter hides is replaced by the **nearest one shown**, in channel order, and
  when it was the selection the status line says why — "Not pinned", "Filtered out", "Hide read
  hides it". Turned back without a scroll, the filter is a round trip: the message comes back where
  it was;
- a message older than everything loaded — a pin from earlier in the day, chosen under Pinned — is
  **walked back to**, at most ten pages, while the head of the list says *Loading earlier
  messages…*, and then put where it stood. A reader who scrolls, presses in the list or jumps
  somewhere meanwhile ends the walk where it is, and is not pulled back when a page lands;
- the selection goes with it, lit and focused as it was.

Nothing else moves it afterwards until the reader scrolls or does something in the list: not a
message arriving at the newest line, not a refresh landing behind the change, not the chips under the
list changing height, and not a thread's landing on its first unread message. The reader's own act
ends that — a press or a tap in the list, another message chosen by the keys, the mouse or the focus,
a key that acts on a message, a fold opened or closed, **Collapse all** or **Expand all** — and from
then on the page keeps their place as it does everywhere else, from where they are. So a message
folded just above the kept one stays where the reader saw it, and a reader on the newest line who has
moved on is followed down to new messages again. A view switch is not a filter, and keeps its own
landing rules. At the head of the Pinned list nothing steps back through the channel's history
behind it: older history adds no pin to that list.

### Only the links

`#211 link-filter`. The chain-link button in the open search bar, immediately left of Pinned, turns
on the **Links** filter: the list keeps only the messages that hold a link, and each of those shows
**its links and nothing else**, one per line, under its author and time. Every one is a real link
that opens in a new tab; an address longer than the screen is cut short with an ellipsis while the
link itself stays whole, and a long press shows where it goes. A tap on a link opens it and does
nothing else to the row — no fold, no read-aloud in reading mode, no details sheet.

It reads **what the search reads**: the messages loaded in the view on screen — the channel in
Main, All or a thread, with whatever Hide read leaves out already gone, or the voice transcript —
and nothing is fetched, so the count says "1 of 13 loaded" for the reason the search's does.
Every filter in force has to hold: typed text narrows the rows with links to the ones whose message
matches it, and with Pinned on as well it is the pins that hold a link. When nothing is left the
list says which answer it is: no links in what is loaded, none of the pins has a link, or none of
the messages with a link matches the search. Closing the search bar turns it off, as it does
Pinned.

**A message the server rendered lists the links it draws** — its anchors, by the names they are drawn
with — so the Links view and the row never disagree about what is a link (`#217 markdown-blocks`;
see [How message text is drawn](#how-message-text-is-drawn)). A GitHub address drawn with a short
label is listed by that label (`#227 github-link-abbrev`); the kinds below still read its address. The rules below are for text no
server rendered: a voice turn, a message being sent, a message saved before rendering existed.

Only **http and https** addresses become links; `javascript:`, `data:` and anything without a scheme
never do. Three forms are read: a bare address, with the sentence's punctuation after it left off
and a closing parenthesis kept only when it pairs with one inside the address; the angle brackets
chat services send, `<https://…>` and `<https://…|name>`; and a Markdown link,
`[name](https://…)`. A Markdown or angle-bracket link shows its **name**. A Markdown link that is not
complete and well formed — brackets that do not pair, an empty name, a target that is not http(s) —
shows the address written inside it, if there is one, and never a name. An address that appears
twice in a message, or in the messages one combined row stands for, is one line.

**PRs, Commits, Actions, Issues, Files and Other** (`#212 link-filter`, `#266 link-filter-kinds`).
While Links is on, six toggles hang just under its button — centred under it, or right-aligned under
the end of the bar where centred would run off the screen — each a full 44px target, all on until the
reader turns one off. Every link is of exactly one kind, so between them the six cover every link,
and turning one off takes that kind out of the view:

- **PRs** — a pull request, `/OWNER/REPO/pull/N` and anything under it (its files, its checks, a
  comment), and the `/pulls` lists;
- **Commits** — `/commit/SHA` (seven hex digits or more), `/commits/…`, `/compare/…`, and one commit
  seen from its pull request, `/pull/N/commits/SHA`;
- **Actions** — `/actions/…` (runs, jobs, workflows), a check run's `/runs/ID`, and `/checks`;
- **Issues** — `/issues` and anything under it: an issue, a comment on one, a new one, a list;
- **Files** — a file or a folder of the code at a branch, a tag or a commit: `/blob/REF/…`,
  `/tree/REF/…`, `/raw/REF/…` and `/blame/REF/…`, and a file on `raw.githubusercontent.com`;
- **Other** — every link none of those claims: a document, a wiki page, a release, a discussion, a
  repository's own page, and anything on any other host. It is the catch-all, so turning the first
  five off leaves exactly the documents.

The first five are read from addresses on `github.com` or `www.github.com`, in any case, inside a
repository (and, for Files, `raw.githubusercontent.com`); the query, the fragment and a trailing
slash are ignored, and a Markdown link is judged by its address, not its name. A row left with other
links shows those; a row left with none goes, and the count says so. The number beside each name is
how many links of that kind the messages in view hold, on or off, so a reader can see what turning
it back on would bring ("99+" past ninety-nine), and the six numbers add up to every link in view.
On a phone the six with their numbers are wider than the screen, so the row stays inside the bar and
scrolls sideways: a swipe across it brings Other into view, and a button reached by Tab or a tap is
scrolled wholly into view. When nothing is left, the list names the kinds that did it — "No links left
once PRs and Other links are hidden — turn them back on under the Links button." — and over Pinned or
a search it says the same of the pins or the matches. The choice is kept on the device, as the page's
other view preferences are, so a reader who always turns them off does it once; with nothing kept,
every kind is on, and a choice kept before Issues, Files and Other existed still holds with those
three on.

### Choosing a thread sits beside choosing the channel

`gchat-thread-selector`. The second picker on the control bar, immediately right of the channel
picker and always there on the channel view, chooses what part of that channel to read:

- **Main** — messages posted to the channel itself, with each thread's first message;
- **All** — every message, threads included, in one history;
- then **about one phone screen of threads** (eight), most recently active first, each as
  `age · replies · name` — for example `1d18h · 16 · Release checklist`;
- and, when the channel has more threads than that, **… Older threads**.

A thread has **one name everywhere**: the summariser's display name when the server has one, else
the provider's title, else its first message's first line. The picker shows it shortened; the
heading over an open thread shows the same facts and the same name with more room, and both change
together when a display name arrives later. Ages use one compact scale: `now`, `59m`, `23h`,
`1d18h` (a whole day is just `3d`), `12d` from ten days, `6w` from four weeks, `2y` from a year.

**… Older threads** opens the **Threads screen**: every thread in the channel, newest activity
first, named exactly as the picker names them, with the server's one-sentence summary under each.
**Load older** reads the next page of the channel's thread list; a failed read says so, keeps the
rows already listed, and offers **Try again**. Choosing a thread opens it on the channel; the back
arrow returns to the line you were reading. The screen only reads, so a read-scope token can use it.

The floating chips over the list (Summaries, Undo, Expand all, …) keep their height clear under
the channel composer, so scrolled to the end they never cover the box you are typing in.

Opening a thread lands on its **first unread message**, expanded, just under the floating pill, with
the thread's first message above it when both fit in the upper half of the screen (`#221
read-modes`). A thread with nothing unread opens on its newest message, folded, as before. That
happens once, on entering: a message you fold again stays folded through every refresh after.
A thread that began before All's window is drawn at once from the replies the page already holds
(`#220 view-switch-instant`) and lands on the first unread among them, expanded. Those replies never
include the thread's first message, so the read behind them brings it in, with the older replies,
and the landing is made again as though they had all been there from the start: the first message
goes to the head when both fit in the upper half of the screen, as above (under Collapse read: the
first message, then "… 12 read messages …", then the expanded message), and otherwise the unread
message stays exactly where it landed. Either way it stays expanded, even when the read joins an
earlier message of the same author onto its row, and another read that overtakes this one (a live
message, the poll) keeps the landing too, as you left it: once you have opened the read messages
under the first message, no read takes you down past them to the unread one. When none of those
replies is unread, the landing waits for that read, and does not move you if you have scrolled
meanwhile, or if that read fails.

The threads it lists come from what the page already holds, so opening it costs no request.
Touching it also reads the channel's thread list in the background, at most once a minute, so the
next look names every thread. On a channel whose provider has no thread timelines
(`thread_api = "off"`), the picker stays, offers only Main, and is inert. The Main / Threads / All
tabs that used to sit over the list are gone.

### Choosing the channel is a control, not a line of the scrollback

The picker used to be a row at the **top of the scrolling region**, above the log. That is the
whole of the defect that was reported against it: the control for choosing *what you are reading*
scrolled away the moment you read anything, and getting back to it meant scrolling a channel to
its beginning.

**Paging made it worse, and that half was ours.** Once reaching the top loaded another page,
scrolling toward the picker prepended history above it — so it receded as you approached, and on a
channel of any size it could not be reached by scrolling at all. `#83 channel-selector-in-bar`.

So it moved onto the **control bar**, which `#58 control-bar` built as a packing container for
exactly this, and the fix is placement rather than tuning: with the picker out of the scrolling
element, the automatic step back is free to keep working as designed and nothing here touches
`OLDER_TRIGGER_PX`.

It is the **first** member of the pack, and that is the reachability argument rather than a taste
one — the pack scrolls sideways when it is full, so the member at its left edge is the one on
screen without scrolling anything. It may shrink but never grow (`flex: 0 1 auto` against the
`flex: 1` every `select` inherits from `style.css`), because a member that grew would take the
width the pack holds for the buttons beside it.

**Every member of the pack says which views it belongs on**, in one table (`PACK_VIEWS` in
web/voice.js) rather than in a branch. The rule is where the member's effect lands: the picker
names what the channel view is reading; Prompts goes out through `sendUserMessage`, which draws
the line into the **transcript** and needs a live call, so on the channel view it is a control
whose whole result appears on a screen you are not looking at (writing to the agent is not on the
bar at all since `#241 voice-composer`: its box is in the transcript); and Read new decides what
happens to an arriving message *during a call*, so it belongs where the call is. A member with no
line in the table is hidden everywhere and the page suite names it; there is no default, because a
default is how a control ends up on a view nobody chose for it.

That is a layout decision as much as a semantic one, and it is **measured**: the bar is one strip
on a 375px phone, six controls do not fit on it, and the pack scrolls — so a control pushed past
the pack's right edge is exactly as unreachable as the picker used to be. "The bar fits on a
375px phone" costs whatever `renderControlBar` really leaves visible against widths read
out of web/voice.css, on each view and in text mode, and the same test proves it is not vacuous by
showing that the strip as it stood before the prompts tray — every canned prompt a member in its
own right — overflows by about 55px. That arithmetic is also **why the tray came first**: Read new
is on the strip at all because collapsing the prompts gave a slot back, and it is a cycling button
rather than a `<select>` because a `.bar-select` needs 5rem the call view does not have.

**Refresh stays where it is.** It is a re-read of what is already on screen rather than a choice
about what to read, it keeps your place, and a keyboard reaches it wherever the list is scrolled
to.

The screenshot state `30-channel-picker-in-bar` is the check written for the acceptance criterion,
because "unreachable" is ultimately a measurement a layout engine makes: it walks the channel back
several pages, parks mid-history, and asserts the picker is inside a 375px viewport on all four
sides and did not move by so much as a pixel across the whole walk. **It has not been captured.**
No host this has run on has had Playwright or Chromium, so what is checked today is the offline
arithmetic above and `--self-test`, which confirms the scene is *pinned* to those claims without
rendering a frame. The same is true of `31-pull-to-refresh-armed`. Both are written and neither is
evidence yet; the first run with a browser is what turns them into any.

### What to call a channel

`#39 channel-alias`. A channel arrives with two names and neither is sayable: the snowflake
`1532416065114607829`, and whatever `label` was written in the configuration file — which is often
something like `build noise`, chosen when there was one channel and nobody was addressing
it out loud. The motivation is the next feature rather than this one: with several channels
configured, *"ask the build channel"* has to resolve to something, and a short name you chose
yourself is the only candidate.

So you can **give a channel a name of your own, in this app**, from Settings. The alias wins
wherever the configured label showed; the label is the fallback, and clearing the alias returns to
it. `ChannelInfo::display_name` is the one place that rule lives, so the picker, the head of the
channel and the digest header cannot come to disagree.

**Both served pages, not just the one with the editor.** The alias is set from `/voice` Settings,
but it is shown at `/` too — both channel pickers there and the channel-name header — because two
pages of one deployment showing two names for one channel is precisely the disagreement
`display_name` exists to prevent. In the browser the rule is a function called `channelName`, and
it exists twice: `/` and `/voice` are separate assets with no build step between them, so they
cannot share a module. What holds them together is a test in `src/http/api.rs` over the bytes of
**both** files, asserting that each defines the rule and that every place either one names a
channel goes through it. That guard is not new — it was widened after `#52 operator-timezone` and
`#62 message-count-accuracy` were each fixed on one page and carried across afterwards — and
`#39` is the third time the same trap was walked into. `tests/js/app_page.test.mjs` is the
behavioural half for `/`, alongside the long-standing suite for `/voice`.

**It is ours, and it is local.** It lives in the `#64 storage-backend` store beside the read
state, exactly as `#50 todo-view`'s "dealt with" marks do — the same posture, and for the same
reason. It is never sent to Discord, renames nothing there, and nobody outside this deployment
sees it. Every answer that sets or clears one carries a standing statement saying so, and `/voice`
shows *that* sentence rather than one of its own.

**The voice agent hears it too.** `list_channels`, the digest header and the tool manifest all
name the channel the way you do, so the name you say out loud is the name the model was
given. That is a read of local state on the way out; nothing about it is a write to Discord.

**The agent cannot choose it, and the reason is not the scope.** A hosted voice agent is routinely
given the write token — `post_reply` needs it — so gating a rename behind the write scope would
not keep a model out, and this README is not going to claim it does. What keeps it out is that
**there is no tool**: the MCP manifest offers none at either scope, and `dispatch` refuses any name
it cannot find there. `tests/alias.rs` asserts both, with the write token, and the page suite
asserts the browser half — nothing arriving on the conversation socket reaches the alias route.

Retention deliberately does not reach this table. There is at most one row per configured channel,
so it cannot grow with use, and an age bound would be worse than absent: it would put the
configured label back months later with nothing on screen saying why. `DELETE /api/v1/storage`
still takes it, because "erase everything" has to mean everything.

Out of scope, and stated so nobody reads more into it: the agent changing an alias, renaming
anything in Discord, and multi-channel addressing itself — which this only prepares for.

### Taking a channel off the list

`#199 removable-config-channels`. Every channel in the list can be removed from Settings, including
the ones the configuration file names. The owner could not remove the deployment's one configured
channel: the server refused to delete a channel its file names, and the page did not offer the
button.

**One verb, two meanings, decided by the server.** `DELETE /api/v1/channels/{id}` on a channel
added in the app forgets it, as before. On a configured channel it **hides** it instead. The server
reads that file and never writes it, so deleting the channel would last until the next restart and
then undo itself. A hide is a row in the store, and it lasts. The file still says which channels
exist; the store records which of them you took off your list. Without a store the hide is refused
with `storage_not_configured`, rather than held in memory until the next restart.

**Hidden means gone everywhere, not just unlisted.** A hidden channel leaves `GET /api/v1/channels`,
client-config's `channels`, the inbox, the live poller, diagnostics, and everything the voice agent
is told. Every channel-scoped route answers `unknown_channel` for it, exactly as for a channel the
file never named, so nothing reads it, posts there, or streams it. All of that follows from one
place: `AppState::channel` and `AppState::all_channels` subtract the hidden set, and every consumer
already goes through them.

**Getting it back.** Settings lists *Hidden channels from this server's configuration*, each with
**Show again** (`DELETE /api/v1/channels/{id}/hidden`). Adding it again also works, by its id or
from Browse channels, where a hidden channel is offered like any channel not on the list. Either
way the file's label and write policy apply, not the ones typed when adding it. Adding from Browse
still asks the bridge to resolve the link, as every add does, but nothing is ever unregistered
upstream for a configured channel, and hiding one touches nothing upstream at all. client-config
sends the hidden list only to a write-scope caller, because only that scope can show one again.

**With every channel hidden** the list is simply empty. The page says so and points at Settings,
where *Add a channel* and the hidden list are. The server answers every listing with an empty one.

**Only the owner edits the list.** Both routes take the write token, and there is no MCP tool for
either, for the `#39 channel-alias` reason: the list is the owner's. `tests/hidden_channels.rs`
asserts that with the write token.

What it does not change: `[[channels]]` must still name at least one channel. A deployment can
reach an empty list at runtime, but a file with no channels is still refused, because that is far
more often a configuration mistake than a deliberate choice. The startup probe still reads every
channel the file names, hidden or not, because it checks the file, not your list. An id hidden and
then removed from the file stays recorded, and hides the channel again if the file names it later.

### Messages read automatically

`#196 auto-read-noise`. A coding agent posts `_Working…_` as a thread reply while it works, and the
real answer arrives later as a separate message. In the owner's main space nine of the latest fifty
messages were exactly that, and each one was an unread row, a to-do item, and a line of every
digest. So the server keeps a short list of **noise rules**, and a message matching one counts as
read without anybody tapping Done. The list starts as one rule, `Working…`, and is edited in
Settings under *Read automatically*.

**The matcher is deliberately trivial.** A rule matches when the message's WHOLE text equals it,
ignoring case, extra whitespace, `_ * ~` emphasis around it, and `...` versus `…`; a rule ending in
`*` matches how a message starts instead. A message that merely begins with "Working…" and goes on
is a real message and is not matched. Not a regular expression, on purpose: a rule hides messages
from the person it is for, so what it catches has to be predictable by reading it.
`src/noise.rs` holds the matcher and the one sentence Settings quotes about it.

**Evaluated on every read, never recorded.** Each message the server serves is judged against the
rules as its text reads now and carries `noise: true` when it matches — the page, the to-do list,
the count, the digest and the agent's tools all read that one flag. Nothing is dismissed — not
even by **Clear the backlog**, which sweeps only the messages the to-do list showed and so never a
placeholder — so a placeholder later edited into the answer is unread again, and removing a rule
brings back everything it caught.

What "read" means here, concretely: `/todo` leaves noise out and says how many (`noise`); `/count`
and `/digest` skip it and say how many; the agent's `digest_channel`, `read_page` and
`count_messages` leave it out and say so, and `find_message` never offers one as a match; read-aloud neither prepares it nor relays it into a
call; and the page dims it, gives it no unread treatment, and hides it under *Hide read*. A tap on a
placeholder still reads it aloud — an explicit request is not noise. `/inbox` holds only read marks
and counts no messages, so it has nothing to skip.

**A false positive is rescued per message.** *Not noise* on a dimmed row records that one message
on the server (`POST /api/v1/channels/{id}/not-noise`); the rule goes on catching the placeholders
it is right about.

**Only the owner edits the rules.** `PUT /api/v1/noise-rules` takes the write token, and — for the
`#39 channel-alias` reason — there is no MCP tool for either route: a model that could add a rule
could hide messages from the person it reports to. With no store configured the default rule still
applies and an edit is refused; with a store that is failing, nothing is treated as noise, because
a failure should show more rather than hide more.

This is this app's own read state, like Done. The source chat service's read cursor is not moved,
and the upstream-read route is not involved.

### Pinned messages

The owner asked for "our own concept of 'pin message'": a message to find again later. **Pin
message** is in every row's ⋯ menu, after **Copy text**; a pinned row says **Pinned** in a gold
chip beside its time and carries a gold edge on its right. With the search bar open, the pin
button beside the magnifying glass turns on the **Pinned** filter: the list becomes this channel's
pinned messages and nothing else, in the order they were sent, whatever view — Main, All, a
thread — is underneath and whether or not they are Done. Search text still narrows it, and
closing the search bar turns it off with the text. `#206 pin-message`.

**These are this app's pins.** They are kept by this server, so they outlast a restart and show
on every device signed in to it. The source chat service's own pinned messages are not read, and
pinning here pins nothing there. There is no MCP tool for any of it, for the `#39 channel-alias`
reason: the list is the owner's, not the voice agent's.

**A pin keeps a snapshot** of its message — author, text up to 2,000 characters, time, and the
thread it is in — so a pin older than anything the page has loaded is still shown, and opened in its
thread, from that alone; the chip on such a row says it is the kept copy. Wherever the message
itself is loaded, the page shows it instead, edits included.

**Bounded per channel.** A channel keeps at most 100 pins. Pinning one more unpins the channel's
OLDEST pin — oldest by when it was pinned — in the same write, and the page says so; it never
refuses the pin you just made. Pins are not covered by `retain_days`, because an age limit would
unpin a message you pinned in order to keep it; `DELETE /api/v1/storage` erases them.

**Kept current by the reads that already happen.** Every channel read, the incremental refresh's
deltas included, carries the pins' `revision`, and the page reads the list again only when that
changed — entering a channel, after a pin made here, and when another device pinned or unpinned
something. A pin made while the page knows it is offline is refused with a sentence, not queued.

### Pulling the channel down, and the other end of the same container

This came out of a real report — the channel found hours out of date: *"especially when I swipe up
on this view and it shows me something very stale."* The staleness itself was already fixed — the view re-reads on
entry and polls every forty-five seconds while it is up — and that covers being stale *and
waiting*. It gives no way to say **refresh, now**. `#68 pull-to-refresh`.

**The contention is the whole design question**, because pull-down-at-the-top and
load-older-on-scroll-up are the two ends of one scrolling element. The rule is two sentences, and
it has to be two, because the obvious one-sentence version leaves the gesture unreachable on
exactly the channels that need it:

1. **A finger on the glass suspends the automatic step back.** Deferred, never dropped — it is
   taken the moment the finger lifts, so `#65 scrollback-paging` stays automatic rather than
   becoming a button.
2. **The pull is measured from where the list ran out**, not from where the finger landed. An
   overscroll begins at the edge, so the pixels spent reaching the top are scrolling and only the
   ones after it are a pull.

So the reader has one continuous motion for each meaning: drag up through the history and the list
scrolls (lift within `OLDER_TRIGGER_PX` of the top and the deferred step fires, and the walk back
continues); drag down until the list runs out and keep going, and the extra travel past the edge
is the pull. `overscroll-behavior: contain` is what leaves that overscroll to this page instead of
letting the browser reload the whole application under it.

**The one-sentence version was wrong, and it shipped.** Judging the gesture by the scroll position
at `touchstart` reads well — "the list is already at its top, so this drag is an overscroll" — and
it describes a state a reader on a paged channel can never be in: arriving at the top fires the
step back, the anchored prepend puts them at a positive offset again, and so `scrollTop === 0`
never holds until the whole history has been walked. The feature existed only on channels short
enough not to need it. Rule 2 is what replaces that reading while keeping what it got right: a
flick started a little below the top runs the list out within a frame, so the first `touchmove`
already reports zero — anchoring at the edge means that flick has travelled nothing yet, where
judging it from where the finger *is* would turn ordinary scrolling into a refresh.

Two more rules keep the features out of each other's way: a step already **in flight** refuses to
arm a pull (its answer is about to prepend content above the viewport), and an **armed** pull drops
the deferred step rather than taking it on release — the reader asked for the newest end, and
answering with more history is answering the opposite question.

A pull is also **one finger going down**. A drag whose sideways travel exceeds its vertical travel
is refused for the rest of the touch (the platform's own edge-swipe carries tens of pixels of
downward drift), and a second finger landing mid-gesture refuses it too rather than re-reading
`touches[0]` and silently restarting the measurement from wherever finger one has reached.

**It says what it is doing, and then what it found.** *Pull to refresh* while dragging, *Release to
refresh* once past the threshold — before the finger lifts, so the gesture can still be abandoned —
and *Refreshing…* while the read is out. Then one line: *refreshed — something new had arrived*, or
*refreshed — nothing new since the last read*. A refresh that finds nothing must not look like a
refresh that never happened.

**A pull goes to the newest message; the button keeps your place.** That difference is deliberate:
a pull is made *at the top of the history* asking for what is new, and keeping your place there
means staying at the oldest thing loaded. **Refresh** (`#refresh-discord`) is pressed from wherever
you are reading, so it keeps you there — and it stays, because a desktop has no gesture and a
keyboard reaches a button.

### One channel row at a time, and one look for it

A long channel is a wall of blocks, and the row the reader is on is picked out: its surface turns
`--row-hover`, its edge and a ring take the accent, its controls come up to full contrast, and a
dimmed row comes back to full strength.

That row used to be lit two ways, by `:hover` under the mouse and by the focus when the keyboard
selected a message, and on a desk the two parted: a message lit by the mouse, then ↓, left two
messages lit, with nothing to say which one Enter would open (`#239 single-selection`). So the mouse
and the keys move **one** selection per list, and the selection is the only thing that lights a row:
`data-selected`, on one row of the page at most, drawn the same whoever chose it.

- A key that moves the selection takes it from wherever the mouse put it.
- A real mouse move over a row selects that row. The focus goes with it when the focus was on a
  list, on nothing, or on a control a click focused: a click on Summarise or Show read leaves the
  focus on the button with no ring, and if it stayed there Enter would press the button again while
  the message lit waited. The focus never leaves a box being typed in, a picker, an open ⋯ menu, or
  a control Tab reached (one drawn with its ring), whose reader is working by keyboard. A move is
  real only when the pointer itself moved: a list scrolled under a still pointer, by a key or by a
  message arriving, takes nothing.
- A click selects too. A tap on a touch screen selects **silently**: no focus, nothing lit. A touch
  screen has no hover, and a row lit by a tap could never be put out, which is the property the
  first version of this treatment (`#56 message-hover-highlight`) was built around. No row has a
  `:hover` rule any more, so no phone can get one. A touch screen keeps the tint it always had on a
  row whose control a finger focused, and so does a touch laptop, which answers the pointer queries
  as a desk does: `data-input="touch"` on the root element, from a tap until the mouse moves or a key
  is pressed, gives it the same tint.
- The row a reply's arrow or circle jumps to becomes the selection. Its flash is on the message every
  key now acts on, and goes out when the selection moves on.
- In two columns each column keeps its own selection, and only the column in use draws its one.
  Moving the mouse into the other column takes the selection, and the keys, there.

What a pointer still lights inside the lists (the disc behind a reply's arrow, the circle's halo) is
behind `@media (hover: hover) and (pointer: fine)`, and held back while the keys are in use:
`data-input="keys"` on the root element from any key (Escape out of the search too, which the field
handles itself) until the mouse really moves.

The row's actionable child is the reply control, and selecting the row picks that out too. It is not
*revealed* by hover the way Discord's row actions are: the standing rule on this page is that a
control which cannot act is absent, so a control that can act must not be invisible.

Contrast is the real deliverable here and no test can judge it, which is why `20-discord-hover`
sits beside `10-discord-view` in the screenshot set: the pair is the with-and-without evidence, and
it was reviewed in both schemes because a tint that reads on white can vanish on `#1d2026`.
`tests/keyboard_browser.py` counts the rows Chromium draws lit while a real mouse and real keys take
turns, and requires one.

**Selected and read is ringed grey** (`#245 done-feedback`). A read message comes back to the lit
surface and full strength when it is selected, so it is easy to read again, and a lit read message
used to look exactly like a lit unread one: marking the selected message Done with `e`, whose
selection then moves on to the next unread message, changed nothing the eye could find. So the ring
says which it is, by one rule wherever a row is selected: a **solid** ring in the accent for unread, a
**dashed**, lighter grey one for read (Done, your own when that setting is on, or noise), on the same
lit surface. Dashed and not merely grey because in the light scheme the grey ink and the blue are both
dark and differ only a little in hue; the light scheme's read ring has its own lighter grey that still
stands at 3:1 against the lit surface. The moment of marking is drawn on the row that was marked,
though the selection has left it: its ring turns from the solid accent to the dashed grey over a
quarter of a second while the row fades to the read look, holds for most of a second, then lets go.
The Done button, a swipe, `e` and Done through here (on the row it was asked from) all draw it, and
every way back ends it at once: a Done the server refused, the Undo chip, `z`, the row's own button,
or another device putting the message back — a row drawn unread never wears the grey ring. It is an
animation from the selected look rather than a transition, because on a threaded channel a Done
redraws the row as a new element; under `prefers-reduced-motion` the grey ring and the read look
arrive at once. The way back is the Undo chip that was already there, which now names the message it
would put back (`Undo “seeded: filed two issues…”`, or `Undo Done (12)` after Done through here)
where the row of chips has room for a name. On a phone it says only **Undo**: beside Newest,
Summaries and Expand all at 412 pixels the name gave way to one letter, `Undo “B…”`, which names
nothing. So a row of chips narrower than 30rem (every phone, and a desk's column dragged narrow)
draws the plain word, and wider the chip shows at least a dozen characters of the name, grows into
what the row has spare, and cuts the rest short with its own ellipsis inside the quotation marks.
Either way the tooltip and a screen reader have the whole name.

**The chips never stand on the message box.** They float over the foot of the list, and the
channel's message box is the last thing in it, so as the list scrolled the box and its Send passed
under them — the Undo chip stood on Send in the 412-pixel captures, the list a little short of its
end. Now the chips stand no lower than the box's top edge: scrolled up they float at the foot of the
list as before, and as the box comes up the screen they stand on it and rise with it, the way a
sticky row does, and the status pill and the pull's report rise with them. At the very end they stand
in the room the box keeps for them above itself, between the last message and the box. A channel too
short to scroll, whose box ends above the strip they float in, leaves them at the foot, where they
cover nothing (lifted over it they stood on its last rows and the menus those open). The call's
message box keeps its room under itself, where the status pill it speaks on at every turn floats.

`tests/done_feedback_browser.py` reads both rings, their contrast in both schemes and the running
animation by computed style in Chromium, takes an Undo inside the hold, measures the row of chips at
360 and 412 pixels (a plain Undo, taking no line a bare one did not) and at 1280 (named, on one line),
and at all three walks the list up from its end a few pixels at a time through the whole message box,
requiring that no chip and no status pill ever meets the box or its Send; `53-done-feedback`
photographs the moment.

### Replying from the page

`POST /api/v1/channels/{id}/reply` existed from the start and, until now, had exactly one caller:
the voice agent. So a reply typed by hand did not exist, and nothing could tell an answered message
from an unanswered one — Discord records a `message_reference` only for replies made through the
affordance.

Every raw message in the channel view now carries a **Reply** control, and it opens a screen of its
own rather than a box wedged under the list: the message being answered has to stay legible while
you write, and on a phone a text field beneath a scrolling list means the keyboard covers both.
Three decisions worth stating, because the issue left them open:

- **The posted reply is appended, not re-fetched.** The server hands back the `Message` Discord
  accepted, which is better evidence than a later read — and a refetch replaces every child of the
  log, which would destroy the anchor the reader's position is measured against.
- **Drafts are per message**, and mirrored to `localStorage` with the same read-after-write check
  the token uses. One global draft would silently hand what you wrote about one message to a reply
  to a different one.
- **A send that fails changes nothing.** The text stays in the box, the reason appears beside it,
  the reader stays on the screen, and — because posting is not the call — a live conversation is
  untouched.

Coming back from the reply screen lands on the same line you left, and it does that with the same
anchor-and-offset mechanism the fold control uses rather than a saved `scrollTop`: hiding an
element is allowed to reset its scroll position to zero, and a restore expressed as a delta against
an anchor is correct either way.

**The reply screen keeps the message where it was** (`#247 reply-continuity`). The owner found it
jarring that Reply changed the font size and moved the message, and asked for the same visual
continuity as other transitions. Now:

- **The message is drawn as the list draws its row.** It uses the same renderer, the reader's
  Settings text size, the speaker's tint and insets, and a reply's gutter or a gathered reply's
  indent where the row had one. It sits in the same column on a desk, so its lines break in the same
  places. It is unfolded, and an accent edge marks it as the one being answered. Who it answers and
  the id the answer goes to sit beside "Your reply", not above the message.
- **Its top stays where the row's top was.** The conversation scrolls to put it there, and ends at
  the message's foot, so the box comes up directly under it and nothing that came after it on the
  list is shown. The earlier messages are above it as before. A row whose top was above the screen
  (under the list's edge, or where the title bar now is) opens at the top.
- **It is never pushed under the box.** If the message ends lower than the box leaves room for, it
  moves up only as far as needed to show all of it above the box. A message taller than the screen
  keeps its top, with the rest a scroll down, as long as its first third of a screen shows;
  otherwise it moves up just enough to show that. Where too little came before the message to fill
  the space above it, an empty room at the head of the conversation holds it in place.
- **A resize places it again, from the same place.** A window made shorter or taller, or a phone
  turned on its side and back, re-runs the same rules from where the row's top was when Reply was
  pressed: in a short room the message moves up only as far as shows it over the box, and back at
  the size Reply opened at it is where it began. A reader who had scrolled back up the conversation
  is brought back to the message the same way. A phone's on-screen keyboard is not a resize in
  Chrome on Android or Safari on iOS: there it pans the whole page over the screen, and the message
  moves with everything else.
- **A subtle tint**, the page colour with a little of the accent in it (one value per scheme), says
  this is Reply. Nothing else changes.

Cancel, Back and Esc come back to exactly where the list was. Send still goes to the newest message,
and `b` or the Bookmark chip brings you back. `make -C vibe-talk reply-continuity-browser` measures
all of this in Chromium at 1280x800 and 412x915, at three text sizes, then resizes the window (and
turns the phone) with Reply up, and sends a reply.

**In two columns there is no reply screen** (`#277 reply-in-column`). The owner: "Reply and
quote-reply destroy two column mode. Again I expected minimal disruption. Reply in the right column
or left column, whichever I clicked the reply button in." So in two columns, Reply, Quote reply, a
row's Draft and a failed reply's Edit open the reply screen's own box — "Your reply" and the id it
answers, the box, the new-thread choice and where it posts, Cancel and Send reply — at the foot of the
column the button was in, under that column's list. It is the same box, moved (`dockReply` in
`web/voice.js`), so where a reply goes, the quote, the draft kept on Cancel, Enter and Esc are exactly
the reply screen's. Both columns stay on screen; the other one does not move, and the pressed one keeps
its top where it was and is shorter by the box, its chips standing on the box. The message answered
stays where it is, edged in the accent; only where the box would cover it does the column scroll, by as
little as shows it. Send, Cancel and Esc put the box back and the column where it was when Reply was
pressed, with the keys on its list. A column that changes under an open reply (another channel, thread
or view) puts it away as a draft. One column — a phone, or a desk with the layout button off — keeps
the reply screen: on a phone a box under the list is what the on-screen keyboard covers, and the
one-column desk keeps the continuity above. A window narrowed to one column while a reply is open in a
column hands it to the reply screen, words and all. The other way round is left alone on purpose: a
window widened while the reply screen is up keeps the reply there, the screen you are writing on, and
two columns come back when it closes.

Because Send from a column stays where you were, a reply that does not go would otherwise be
invisible: its outbox row, with Retry, is down at the column's foot. So the status line says so — "Your
reply was not sent. Retry is at the foot of the left column.", or "may not have been posted" where
delivery is unconfirmed, and a screen reader hears it — and that column's Newest chip reads **Reply not
sent** (or **Reply unconfirmed**) in the outbox's orange until you have been down to the row; pressing
it takes you there. A reply the page will send again by itself (offline, or after a provider's blip
where retrying cannot post twice) is said to be waiting, and the chip changes only if it finally
fails. Esc on the other column's list acts on that column and leaves the reply alone. The box can be
dragged taller only to a quarter of the window, so the list over it keeps its room.
`make -C vibe-talk reply-in-column-browser` measures all of this in Chromium at 1280x800, a refused
reply from each column included.

### Enter sends, in every message box, until you say otherwise

Enter used to mean three things. The channel's box (and so a thread's, which is the same box) sent on
Enter, the call's box sent on any Enter, and the reply screen's box had no rule at all, so Enter
there was a new line and sending meant reaching for the mouse. One handler now decides for all of
them, and **Settings → Typing → Enter to send** decides what it decides:

| | Enter | Shift+Enter | Ctrl+Enter / Cmd+Enter |
|---|---|---|---|
| **On** (the default) | sends | new line | sends |
| **Off** | new line | new line | sends |

Ctrl+Enter and Cmd+Enter send either way, so turning the switch off never leaves a box without a
key that sends. Under the switch, the hint names that key in the words on your own keyboard: Cmd on
an Apple one (from `navigator.platform`), Ctrl elsewhere. The platform only picks the words; both
keys send everywhere. An input method that is still composing a word (`isComposing`, or keyCode 229)
keeps its Enter, because there Enter accepts the word. The call's box is a single line, so with the
switch off its Enter does nothing rather than being handed to the browser, which would submit the
page. One press is one send: the repeats of a held key never send, and where the press itself sends,
they type no new lines either.

The choice is stored on this device under `vibe-talk.voice.enter-sends`, like the page's other
settings. Nothing is added to the boxes themselves, so a phone screen loses no space. Instead each
box's `enterkeyhint` follows the switch, which is what the phone keyboard's return key shows: a send
arrow when Enter sends, a return arrow when it starts a new line.

### The keyboard: scrolling, search and Settings

`#224 keyboard-shortcuts`. On a desk, in a browser tab or the installed app, the list on screen
scrolls from the keyboard the way any web page does:

| Key | Does |
|---|---|
| PgUp, PgDn, Space, Shift+Space | Scroll a screenful up or down |
| Home, End | The top of the list, or its newest message |
| Ctrl+Home, Ctrl+End (Cmd on a Mac) | The same from anywhere, a message box included |
| `/` | Open the search with its field ready; pressed again, back to what you typed |
| Enter, in the search field | Search older messages, a few pages at a time; the button at the head of the list says Stop meanwhile |
| `,` or Ctrl+, (Cmd+,) | Settings; pressed again, or Esc, back to where you were |
| Esc | Closes a menu, the search or a sheet; leaves Settings, Help, Reply, Threads or a thread; never marks anything read |

These keys did nothing before, and nothing was eating them. A browser scrolls the box that holds
the focus, or else the document, and this page's document never scrolls: the frame is fixed and each
list scrolls inside it. None of those lists could hold the focus, so after a load, after a Back
button, or after a click on anything in the dock the keys went to the document and moved nothing.
Worse, a click on the view switch left the focus on it, so the next Space pressed it again. Each
list can now take the focus (by script and by a click, never as a Tab stop), the list on screen
takes it whenever the screen or the view changes, and a scroll key that arrives from somewhere it
cannot act hands it the focus on the way past. The browser then scrolls at its own distances and
with its own smoothing.

In a message box, Home, End, Space and the arrows are the box's own. PgUp and PgDn page the list
beside the box while the box keeps the focus and the draft, unless the draft is taller than the box,
when they page the draft. Every control in Settings keeps its value the moment it changes, so leaving
Settings, with Back or Esc, is all that saving needs. A key that types a character (`/`, `,`) never
acts while you are typing in a field, and nothing acts while an input method is composing a word.

`make -C vibe-talk keyboard-browser` presses these keys through CDP `Input.dispatchKeyEvent` in a
real Chromium at 1280x800, so the browser's own default scrolling runs, and measures the list on
screen after each one: on load, after a view switch, in the search, on Settings and in the message
box. Pass `SCREENSHOTS=/tmp/shots` to keep a PNG of each step.

### The keyboard: moving through the messages and acting on them

`#224 keyboard-shortcuts`. On a desk the messages can be read and dealt with without the mouse.
The arrows, or `j` and `k`, move a ring from message to message. The ring is real keyboard focus on
the row, so a screen reader announces the message and its buttons are one Tab away. What a key does
to "the selected message" it does to the one in the ring. **`?`** (or Ctrl+/, Cmd+/ on a Mac) opens
the full list on the Help screen; the list is generated from the same table the keys are answered
from, so it cannot name a key the page does not answer.

| Key | Does |
|---|---|
| ↓ or `j`, ↑ or `k` | The next or previous message. A message taller than the screen is scrolled through first |
| `n` | The next unread message below |
| Enter or `o` | Open or close the selected message, as a tap does (in Read, read it aloud) |
| → | Into the thread of the selected message; in two columns it opens on the right, and the keys go with it |
| ← | Out of a thread, back to the message it was opened from; in two columns, back to Main on the left |
| `[`, `]` | In two columns, the keys to the left column (Main) or the right one (the thread, or its list) |
| Esc | Back (see above). In a message box with text in it, the first Esc leaves the box and keeps the draft; on the channel or thread picker, back to the messages |
| Home, End | The top of the list or its newest message, and select it |
| `e` | Done or not Done; marked Done, the ring moves on to the next unread below |
| Shift+E | Done through here |
| `z` | Undo the last Done |
| Shift+B | Put the bookmark just above the selected message; on the message right under it, take it away |
| `r`, `p`, `s`, `.`, `i` | Reply, pin, read aloud, the ⋯ menu, the message details |
| Ctrl+C (Cmd+C) | Copy the text of the selected message, when no text is selected |
| `1`, `2`, `3` | Main, Threads, All |
| `h` | Show read, Collapse read, Hide read, in turn |
| Shift+P, Shift+L | The Pinned and Links filters |
| `;`, `:` | Expand all, Collapse all |
| `b` or `m` | Back to the bookmark, with the message under it selected |
| `c` | Write to the channel (or the thread you are in); on Reply, the reply; in the call view, to the agent |
| Ctrl+S (Cmd+S) | The same box from anywhere: typing in the search, on Settings or Help too. The browser's "Save page" never opens |
| Ctrl+Enter (Cmd+Enter) | In a message box, send; anywhere else, the message box, as Ctrl+S |
| ↑ in the empty message box | Select the newest message |
| Alt+↓, Alt+↑ (Option on a Mac) | The next or previous channel |
| Ctrl+K (Cmd+K) | Choose a channel; once chosen, the keys are back on the messages |

A click on a message selects it without a ring, so the next key starts there. PgUp, PgDn and Space
stay the browser's, and once the page comes to rest the ring moves to the first message shown whole.
A key about one message never acts on a message scrolled out of sight: it brings the ring to the
first message showing instead, and the next press acts on that. The selection is kept by message id
for each list, so a refresh that redraws the rows puts the ring back on the same message.

The keys that type a character (letters, digits, `/`, `,`, `?` and the rest) never act while you
are typing in a box, and can be turned off under **Settings → Keyboard → Single-key shortcuts**,
which is on by default (WCAG 2.1.4). The arrows, Esc, Enter, PgUp/PgDn, Home/End and the Ctrl or Cmd
chords stay on. The choice is stored on this device under `vibe-talk.voice.single-keys`. Esc never
marks anything read, no key starts a call, and the call's view takes no message keys. The buttons
that have a key carry `aria-keyshortcuts`. Browser history is not used yet, so the browser's Back
still leaves the page.

Ctrl and Cmd both work for every chord on every platform (the owner asked for Ctrl+S on his Mac),
with one exception: in a text field on a Mac, Ctrl and a letter that macOS uses for text editing
(Ctrl+K deletes to the end of the line, Ctrl+A and Ctrl+E go to its ends, and the rest) belong to the
field. There, Cmd+K chooses a channel. Ctrl+S is not one of those keys, so it goes to the message box
from anywhere on a Mac too.

`#248 composer-key`. Ctrl+S (Cmd+S) opened the search from `#224 keyboard-shortcuts` until the owner
asked for it to do what it does in other chat clients: "When I'm using keyboard, I want to jump to text
entry and start typing." It now goes to the box `c` goes to (the channel's, the thread's you are in,
the reply's on the Reply screen, or on the call's view the agent's box under the transcript), from
anywhere, including while you are typing in the search, and leaves Settings or Help first. Going to
the agent's box starts nothing: a conversation starts only when you send. Ctrl+Enter (Cmd+Enter) does the same when the focus is not in a text box;
in a message box it sends, as before. Held down from outside a box, it goes to the box and leaves the
draft there unsent: its repeats arrive in the box, and a repeat never sends. The search keeps `/` and
the magnifying glass, and the Help list says where Ctrl+S went.

### The reading bookmark

`#240 reading-bookmark`. Catching up on a long backlog means keeping your place while you answer
things further down. Each channel has **one bookmark**: a line in the accent colour across the list,
with the word *Bookmark* in it, drawn just above one message.

- **Shift+B** puts it above the selected message, moving it from wherever it was. Pressed on the
  message right under the line, it takes the bookmark away. The row's ⋯ menu and its press-and-hold
  details offer the same, as *Bookmark above* or *Remove bookmark*. Setting it marks nothing Done.
- **`b`** (or `m`, the key of the place marker it replaced) goes back to it: the line at the top of
  the list and the message under it selected. The **Bookmark** chip, directly right of *Newest*,
  does the same, and is shown only while the line is off the screen, its arrow pointing toward it.
  A bookmark older than what is loaded is reached by reading older pages, as a reply's arrow is.
- **Reply, then `b`.** Sending a reply goes to the newest message as it always has; the chip and `b`
  bring you back to where you were reading. The message you answered is Done once the reply is sent,
  so a line right above it slides on past it, as below.
- **It advances with your Done, and with nothing else.** A Done (`e`, the Done button, a swipe, a
  message read aloud to its end, or a reply you sent to it) on the message right under the line
  slides the line down past messages already Done to the next one still to read, the one `e` selects
  next; below the newest message when everything after it is Done. *Done through here* (Shift+E) at
  or below the line lands it below that message. Undo puts it back. New messages, putting a message
  back, and reads never move it. A Done in a thread view slides along the thread's rows, but never
  past a message of Main's still to read: the line then goes above that message, where Main's reader
  left off.
- **A long Done through here** goes out in batches of up to 1,000. If a later batch fails, what the
  server took stays Done (and is what Undo puts back), and only the rest is shown unread again.
- **Kept on the server**, so it follows you between your phone and your desk: every channel read
  carries it, and a move made on the other device shows at this page's next read. Moving it from a
  Done is conditional on the revision the page last heard, so it can never undo a move made on the
  other device meanwhile. `GET`/`PUT`/`DELETE /api/v1/channels/{id}/bookmark` (read scope to see,
  write scope to change; `if_revision` answers 409 with where the bookmark is now). The anchor's
  `timestamp` must be an RFC 3339 instant, such as `2026-10-12T12:00:00Z`, because a page places
  the line by it where its list does not hold the message. There is no MCP tool for it. The device's
  saved copy of a channel keeps the bookmark too, so a page opened offline still draws the line.

Where the line is drawn: in Main and All above the first message shown at or after its message (a
thread reply is placed in Main by its time), in a thread only when its message is in that thread,
and not in the thread list, the Pinned or Links filters, or while a search is choosing messages.
Under Collapse read or Hide read it drops to the next message shown, since everything it passes is
read. Without threads, Hide read is the server's to-do list, which leaves read messages out: there
a bookmark older than every message shown goes above the first one when the list's window reaches
its message (the to-do answer's `window_from`), and draws nothing when it is older than the window.
A combined row is one box, so the line goes above the row holding any message at or after the
bookmark, never inside it. In two columns the left column draws it, and the right column only on its
thread.

`make -C vibe-talk bookmark-browser` walks this in a real Chromium at 1280x800 and 412x915: set,
scroll away, jump back, reply, jump back again, and a Done advancing the line, measuring where the
line and the chip are drawn. Pass `SCREENSHOTS=/tmp/shots` to keep a PNG of each step.

### Two ways a channel row can already have been dealt with, and only one of them hides it

`#84 reply-aware-dismissal` lands the two follow-ups `#50 todo-view` named for itself — the **swipe**, and the
**derived "replied" signal** that needed a reply reference on the server's `Message`.

`#50` filters the channel on **dismissal**: declared by the reader, recorded on the server, with an
undo and a bulk clear behind it. That stays exactly as it was, and it is still the only filter.
`#66` adds the other half:

| State | Where it comes from | What it does |
|---|---|---|
| **Dismissed** | *declared* by the reader, held on the server | removes the row from the To do list |
| **Replied** | *derived* — some **loaded** message points at this one | dims the row; never hides it |

**`#50` left one question open** — "nothing in this file has to decide what happens when derived and
declared disagree" — and landing the derived half is what forces the answer. **They never meet**,
because they drive different affordances. Declared decides what is in the *list*: dismissing is an
act with an undo behind it, so it may remove a row. Derived decides how a row is *drawn*: replying
dims, and that is all. An observation must not make messages disappear, and this observation admits
it is incomplete.

**That incompleteness is the reason for the split, not a caveat bolted onto it.** Discord records a
reply only on the *answering* message — there is no field at all on the message being answered — so
the server carries the raw pointer (`Message::reply_to`, deliberately not a `has_replies` boolean,
which could only ever have meant "within this fetch window" while being read as more) and the page
derives the rest from what is loaded. A reply further back than the reader has walked is invisible,
so a row can be dimmed *later* than it should have been, never earlier. Being late to dim costs
nothing; being late to **hide** would lose a message.

The derivation is **one pass over the whole list after every mutation**, never a decision taken when
a row is built. Rows arrive from three places, and "has this been answered" is a fact about the
*set*: an answer can arrive in the next poll, and a step backwards can reveal the question a loaded
answer belongs to.

**The swipe drives the same dismissal the Done button does** — one act, one record, one undo. That
ordering was `#50`'s condition for a gesture layer at all, and it still holds: the gesture is a
second way in, never the only one, and a horizontal *mouse* drag is left alone because that is how
a person selects the text of a message they want to quote.

Replying from the page closes the loop on the spot: the reply carries a `message_reference`, so what
it answers dims immediately rather than at the next forty-five-second re-read, and once the reply is
sent the message it answers is marked Done through the Done button's own act (`#256
reply-marks-read`).

### Settings is controls; Help is the paragraphs that used to be in the way

The settings screen had become a document with switches buried in it. Every knob carried two or
three paragraphs explaining itself — all of them worth having, none of them worth reading again on
the way to a checkbox — and on a desktop browser the whole thing ran the full width of the window,
which puts a checkbox at one end of a metre of desk and its label at the other.

So it is two screens now. **Settings is controls and labels**: each group is a bordered card with at
most one short line of orientation and a `?`. **Help holds the prose**, and every `?` opens the
matching entry. Nothing was deleted, and that is the part that makes the split survivable — the
paragraph about the switch you are looking at is still exactly one tap away, which is the only
reason moving it is an improvement rather than a filing cabinet.

Three things follow from that, and each is asserted rather than trusted:

- **The link is checked in both directions.** A settings group that links to an entry which does not
  exist fails; so does an entry nothing links to. The script's `HELP_TOPICS` must equal the markup's
  `data-help` set, so a new group cannot ship with a `?` that does nothing.
- **No paragraph on the settings screen may exceed a budget.** That is the measurable form of the
  complaint, and without it the prose creeps back one sentence at a time.
- **The disclosures are still disclosures.** The tests that guard what resuming and the relay cost
  now assert the control and its summary on Settings *and* the full text in the matching Help
  entry *and* the `?` that reaches it. A test that only looked at Help would pass just as well if
  nothing linked to it.

Both screens are held to a **fixed 34rem column** — not `var(--reading-width)`. That variable is the
reader's choice about the transcript, and dragging the transcript wider has no business reflowing a
form; a form's comfortable width is a property of the form. The desktop block had deliberately
exempted these two screens ("they are forms rather than reading, and giving them a column is a
separate judgement nobody has made yet"); this is that judgement.

**Five sections, not twenty-two cards** (`#271 settings-groups`). With every group its own card, the
screen read as two dozen separate decisions of equal weight, and the owner could not find the switch
that lets the voice agent send without asking — it was a card called "Voice" near the bottom. So
the groups sit as rows inside a handful of sections, and each section is the card:

| Section | Holds |
|---|---|
| **Voice agent** | Sending (ask before the agent sends — first, where it is looked for), Instructions, Resuming, Live messages, Canned prompts, Microphone, and the provider's console link |
| **Messages** | Your own messages, Read automatically, Combining messages, Rich headers, Recently dismissed, How messages are said, Message audio |
| **Channels** | the per-channel box: rename, remove, add, browse, clear unread upstream, who is who |
| **Screen and keys** | Text size, Reading width, Control bar, Typing, Keyboard |
| **Storage and connection** | Stored conversations, Token, Connection status |

A section's title is an `h2` a size up; a group inside it keeps its own small `h3` heading and its
`?`, with a rule above it. No control, `?` or id changed; only where each one sits. The page suite
holds the section list and which groups each section holds.

**Connection status moved to the end and gained a word.** It is a readout — what the conversation
is, what audio format it negotiated, why the last one closed — and it was standing at the top of the
screen, in the position a reader arrives at, when what they arrive wanting is a switch. "Connection"
also read as somewhere you configure one.

**Help is folded** (`#246 help-sections`). A `?` used to land in the middle of one long page. Each
entry is now a native `<details>` whose heading is its `<summary>`, all folded, so the screen reads
as a list of headings, and the way in decides which one is open: a settings group's `?` opens its
own entry and folds the rest, every time (entries opened on an earlier visit do not pile up); `?`
and Ctrl+/ (Cmd+/) open Keyboard, the list of keys; **Open help** at the foot of Settings opens
none. **Expand all** at the top opens every entry and then reads **Fold all**. Because the
disclosure is the browser's own, a heading is a button to a screen reader and to Enter and Space,
and the browser's find (Ctrl+F) still searches folded entries and opens the one it lands in where
the browser supports that: Chromium does (the same mechanism opens a folded entry for a text
fragment, which was checked in Chromium 151), and so does current Firefox; a browser that does not
cannot find text in a folded entry until Expand all. `54-help-sections` photographs Help opened from
a `?`.

### A conversation can end three ways, and they are three different sentences

A WebSocket close is one event, and the page used to have two things to say about it. That is one
too few, and the missing one is the common case on a phone: **put the handset in your pocket
mid-call and iOS suspends the page**, the socket dies, and the page used to greet you on your
return with a red panel saying the connection to the voice agent had FAILED. Nothing had failed.
You switched apps.

`socket.onclose` is now the only classifier, and it distinguishes:

| What happened | What the page says | Dot |
|---|---|---|
| The page was in the background when the socket died | "Paused — the app was in the background." The large control offers **Resume**. | `suspended` |
| Something really broke between this browser and ElevenLabs | The failure panel, unchanged, naming where the fault is | `error` |
| The call ended | What it already said, in words rather than a close code | `ended` |

`socket.onerror` no longer reports anything by itself: it fires *before* the close, and only the
close knows whether the page was hidden. It records the fact and arms a 250 ms timer that the close
cancels — so an error with no close following still reaches the screen, which is what that timer is
for.

**Nothing auto-reconnects.** Returning to a phone that has quietly reopened the microphone and
started a conversation nobody asked for is a worse outcome than the banner this replaces. Resume is
a tap, and it runs the ordinary start path — which mints a **fresh** signed URL every time, so the
"expired credential" case is closed by construction rather than by handling: this page has never
reused one. The clause under the button says so, because "Resume" is the honest word for what the
reader wants and a dishonest word for what the agent gets.

The suspension test is a **heuristic** and is documented as one: a close within 2.5 s of the page
coming back still counts as part of the suspension, because iOS commonly delivers it on the way
*back* rather than while hidden. Too wide a window would excuse a genuine drop moments after a tab
switch, which is why the page suite carries the negative control both ways — a visible failure must
still raise the banner, and a page with the suspension check removed must fail the suspension test.

### The control bar is a container, and it sits where your thumb is

The gear and the Voice/Discord switch used to be the whole of the header. They are now a **control
bar**: one strip that can sit in either of two homes, and that is built to fill up.

**Bottom is the default, and "bottom" means directly above the big buttons** — not below them,
where small icons would sit under the thumb reaching for Talk, and not pinned to the viewport
floor, which on a real handset put the first word of the status line into the corner curvature and
ate it. Top — under the title, where it used to be — is a setting, kept in `localStorage` under
`vibe-talk.voice.bar-placement` and offered in Settings. There is **one** bar and two mount points,
and `setPlacement()` moves the element between them; two copies would be two gears and two switches
for the page to keep in agreement, which is the defect rather than the layout.

**The bar is declared in the dock in `web/voice.html`**, not created from script, so the default
placement is a fact about the served markup: a page whose JavaScript died still shows the controls
where they belong.

**The gear is at the far left and the switch at the far right**, and that asymmetry is deliberate:
the gear is the control you least want to hit by accident, so it goes furthest from where a
right-handed thumb rests, and the switch — the one you actually flick — goes under it. What holds
them at the two ends is `#bar-pack`, the empty container between them, which takes all the leftover
width. That is also what makes the bar **pack**: buttons added to it scroll sideways rather than
wrapping onto a second row (which would spend the vertical space this is all about) or clipping the
switch off the right-hand edge of a 375px phone, where it could not be tapped at all. The switch is
the widest item in the bar, so that is arithmetic, not a worry.

**With the bar at the bottom the header holds nothing on the main screen, so it is hidden
outright** — the grid row collapses and the transcript grows into it. That is the real estate the
whole change is about; an empty 2.4rem strip across the top of a phone is exactly the rent this
project keeps refusing to pay. The header comes back the moment it has something to say: a title
and a way back on Settings or Reply, or the bar itself when the reader has chosen top.

Members hide **individually**, not the bar as a whole, which is what keeps the gear reachable from
the sign-in screen; the bar hides only when every member is hidden.

### The canned prompts, and the one whose wording had to be weakened

Two questions are worth a button because they are the ones actually asked, every time. **Sumry**
asks the voice agent to summarize; **Blockers** asks it to make the *coding agent* report. They go
out through the same `sendUserMessage` a typed turn does, so the sentence lands in the transcript
as the reader's own words, and a tap with no call open reports itself instead of doing nothing.

**They are in a tray, behind one `Prompts` button**, and the reason is arithmetic rather than
taste: the bar is a single 375px row that was priced to the edge, each prompt cost it a member,
and the list is meant to grow — custom prompts are the next thing. One opener costs one slot
however many prompts it holds. The tray is a **sibling of the pack and a child of the bar**: of
the bar because the bar is what moves between the header mount and the dock mount, and out of the
pack because the pack scrolls sideways and a menu that can scroll out from under its own opener is
not a menu. It is absolutely positioned, so it takes no width from the strip at all, opens upward
in the dock and downward in the header, and wraps rather than scrolling. The budget test reads
"out of flow" **out of the stylesheet** rather than from a list, so a tray that stopped being
absolute starts costing the strip real width on the edit that changed it.

It opens **at idle**, deliberately. With no call the prompts grey out, and a reader has to be able
to see what they are in order to learn why they are dead.

They differ in **weight** — Blockers spends coding-agent work on one tap — and that difference
used to be drawn, in the warm tone an armed Clear uses. **It no longer is**, at the owner's
request: on a desktop screen the warm outline did not read as "this one is expensive", it read as
a warning symbol, as though the button were reporting a fault, and a control that looks broken is
worse than one whose cost is only in its tooltip. The weight now lives in the `title` and in the
Settings text. Neither button asks twice — the issue did not ask for a confirm step, so adding one
is a decision for somebody to make out loud rather than drift into, and the suite records its
absence so it is not mistaken for an oversight.

It is a **list**, not two cases: `CANNED_PROMPTS` in `web/voice.js`, one entry per button, and one
loop that restores each field, saves it, and wires its button. A third canned prompt is one entry
plus one pair of elements in the markup, and the suite asserts there is no third place — a button
id named twice in the script fails, because the second mention is the special case starting.

Each prompt is **editable in Settings** and kept in `localStorage`; emptying a field puts the
default back rather than leaving a button that looks live and does nothing. The defaults are
written into `.value` from script, never typed into the markup: text between the tags of a
`<textarea>` is its child text, not its value, so a default written in HTML would be invisible to
everything that reads the field.

**The Summary prompt is deliberately weaker than the one the issue asked for.** It was filed as
*"Summarize my unread messages from the coding agent since I last messaged them"*, and
`#61 unread-status` established that both halves of that scoping are impossible here. Discord gives
a bot no read state at all — it is a client concept, with no ack route and no read-state field. And
the obvious fallback, *since you last spoke*, is not computable either: the digest has no
identity for you, your own replies through this bridge are posted **as the bot**, the digest drops
the bot/human flag, and the only author signal is a display name anyone can set to anything. A button
whose text claims that scoping would produce confident, wrong summaries — the exact failure this
project already paid for once, when an agent invented a digest. So the shipped default asks for
what the data genuinely provides: *"Summarize the recent messages from the coding agent in this
channel."* The suite pins that it says "recent messages" and says nothing about unread or about
when anyone last spoke. Building the capability — an `owner_id` in config, `author_is_bot` carried
into the digest, `before`/`after` on the Discord client — is a real feature and a separate one; the
research is in `ai_docs/UNREAD_STATUS_20260819.md`. Blockers is unaffected, because it makes no
claim about read state.

### Instructions for the voice agent

`#272 voice-agent-instructions`. Settings → **Voice agent** → **Instructions** holds two of the
owner's own texts for the agent:

* **Additional instructions** — a box of standing instructions added to every call: "call me Sam",
  "keep answers under three sentences". Up to 4,000 characters.
* **Edit the system prompt** — folded under it, because it is long and rarely changed: the base
  prompt itself, `prompts/voice-agent-system.txt`, for him to change. Up to 20,000 characters.
  **Restore the shipped prompt** goes back to the file; it deletes his copy, so the first tap only
  arms it (its word and colour change, as Clear's do) and a second within four seconds acts. His
  copy does not follow a newer shipped prompt; the line under the box names the prompt version it
  was saved against when that is older than the one this server ships.

Each box has its own **Save**, a write-scope `PUT /api/v1/voice-prompt`; nothing is kept until it
is pressed, and saving one box leaves unsaved text in the other alone. Both texts are kept in the
store, so every device and every later call has them, and no MCP tool reaches them: a model that
could rewrite its own instructions could drop the rule about untrusted chat text. They are readable
at read scope, because the assembled prompt is. A server with no store shows them and refuses a
save; a store that cannot be read gives the agent the shipped prompt. The access log records a save
as `voice_prompt event="changed"` with the instructions' length and whether the prompt is edited,
and none of the words.

**Neither text can change what the agent may do.** The section that says whether it can send, and
whether it asks first, is still chosen by the server from the token and the Sending switch, and it
goes after the owner's words — base, then his instructions under a short heading saying they grant
nothing, then that section. The server enforces both whatever the prompt says: a read token cannot
post, and with "Ask me before the voice agent sends" on, `post_reply` only proposes. Taking the
untrusted-text rules out of the base prompt is his choice to make, and Help says what it costs.

**How the words reach the agent** depends on the voice provider, and client-config's `delivery`
says which:

* `profile` — a deployment-managed bridge fetches `GET /api/v1/voice-agent` as each call opens, and
  the prompt there is assembled with both texts, so both reach the next call with no change to the
  bridge.
* `page` — a hosted agent (ElevenLabs) is configured in its vendor's console, and this page opens
  its socket. `GET /api/v1/voice-session` asks the vendor, beside the mint, whether the agent's
  security settings allow a client to override its system prompt
  (`platform_settings.overrides.conversation_config_override.agent.prompt.prompt`), and adds
  `agent_prompt` to the session:
  * allowed — `prompt_override`: the page puts the whole assembled prompt, written for a credential
    that may post and following the Sending switch, on the initiation frame as
    `conversation_config_override.agent.prompt.prompt`. **Whether or not the owner saved anything**:
    with nothing saved it is the shipped prompt, so the agent's send rules follow the switch at its
    next call instead of waiting for a prompt pasted by hand. The prompt in the vendor's console is
    then never used. With only instructions saved, the shipped base replaces the console prompt,
    and the line under Instructions says so;
  * not allowed, not said, or the vendor did not answer within 2.5 seconds —
    `contextual_update`: the agent keeps its console prompt, and the page sends the heading and the
    additional instructions as a `contextual_update` straight after the initiation, before a resumed
    call's record. An edited base prompt cannot reach such an agent;
  * not allowed (or not said, or no answer), with nothing saved or only the base edited —
    `not_sent`: nothing is sent, and the initiation frame is exactly what it always was. The
    session says so all the same, with `override_allowed` as read, so that after the call Settings
    can say the agent kept the prompt in its console, and the warning beside the Sending switch can
    say plainly that the new prompt must be pasted there rather than "only if its provider lets
    this page replace it".

  The vendor's answer is kept for two minutes per agent, so most call starts do not wait on it. A
  failure or a timeout counts as not allowed and is kept for twenty seconds: a deployment where the
  read always fails (a key that can mint a session but not read the agent, or a vendor that keeps
  timing out) waits up to 2.5 seconds and logs a warning once in that time, not at every call
  start, and a vendor that was briefly down is asked again soon after. Two minutes is also how
  long an operator who withdraws the permission at the vendor must wait (or restart the server)
  before the page stops sending an override the agent now refuses.

  The override is sent only when the agent's configuration says it is allowed, because an override
  an agent does not allow is believed to end the conversation rather than be ignored. That belief
  comes from the vendor's documentation and has not been observed from this repository; nor has
  whether a `contextual_update` carries instructions as strongly as a prompt does. After each call
  the line under Instructions says what that call was given — above all when an edited prompt did
  not reach it — from what was saved when the call opened, so a save after the call does not
  change what it says the call was given.

  **The override is always the write token's prompt.** This server cannot see which token the
  operator gave the agent's MCP connection at the vendor, so it hands over the prompt for a
  credential that may post. An agent holding only the read token should not be allowed the
  override: it would be told it may send, and the server would refuse every send it tried. Settings
  says this under Instructions on a hosted deployment, and the QUICKSTART says it where the
  override is allowed.

### Typing is the other way to say something, on the same conversation

Speaking is not always available — a quiet room, a commit hash the transcriber keeps mangling, a
name it will never get right — so `/voice` has a text composer, and it is deliberately **not** a
second mode or a second connection. `{"type": "user_message", "text": "…"}` is a client event on
the conversation socket that is already open, documented by the vendor as processed exactly like
speech. A typed turn and a spoken turn are therefore the same thing to the conversation, they land
in the same transcript, and the agent answers either one out loud. On the provider-neutral
`vibe-talk-v1` protocol the same turn is a `prompt` frame, sent once the spoken turn in progress has
been closed — see "Conversational voice providers" — and still no second connection or mode.

**It is a box at the foot of the call's transcript, and it is always there** (`#241
voice-composer`). The owner: "Type mode on voice agent takes away my Voice/Channel toggle ... I'd
actually like to move over to the same metaphor as the channels view. Can we just have a message
box at the BOTTOM of the scroll history?" So the call's view has the channel's composer — the same
box and the same Send, drawn by the same rule, the last thing in the scrolling list — at idle, in
a typed conversation and in a call alike. Writing changes nothing on the control bar: the
Voice/Channel switch, the gear and the rest of the strip stay where they rest, and so do Talk,
Sound and Hang up under it. While the list is shorter than the screen the box sits at its foot,
where a chat's box is; once the turns fill the screen it is simply the last thing in the list. A
half-typed message survives leaving the screen and a reload, and leaves the box only when it is
sent. The keymap treats it as the text field it is: no single-key shortcut fires in it, and Esc
leaves it for the list with the draft kept.

**No mode of the agent's has to be engaged to type.** During a call a typed turn goes down the
socket that is already open, as above. With **nothing open**, the **first send** starts a typed
conversation (below), holds the message while the conversation opens — the text stays in the box
and Send is disabled, so it cannot go twice — and sends it once the server has **accepted** the
conversation, which is later than the socket opening: at `conversation_initiation_metadata` on the
hosted vendor's protocol (after the initiation frame, and the frame vibe-talk's own client of that
vendor waits for before it writes), and as a `prompt` at `session_started` on `vibe-talk-v1`. Until
then the message is still the reader's: not in the transcript, not "Sent.", and still a saved
draft. Clicking into the box, or writing a draft in it, opens no connection and bills nothing: the
conversation starts belatedly, on the send. A message sent while a spoken call is still starting —
asking for its session or the microphone, or connecting — is held the same way and goes into that
call. **One conversation starts at a time**: while one is starting the big control reads
"Connecting…" and pressing it, or Send, starts no second one. A conversation that fails to open,
or that closes before the server has accepted it, leaves the message in the box, says it did not
go, and sends it nowhere, not into the next call. On `vibe-talk-v1` the protocol does not
say whether a server greets a client that never opens an audio segment, so a typed call does not
wait for a greeting; on a server that does greet, the greeting's `turn_complete` is taken as the
first message's. The transcript is still right, because rows are kept per turn number, and a
message sent while the real reply is arriving waits for it — but one sent in the moment between the
greeting ending and the reply starting would go while the first is still being answered. That is
the page suite's pinned "documented limitation", and since this change it is every typed call's
first message rather than a corner.

**The on-screen keyboard resizes the page.** The frame is a fixed `100dvh` grid with the dock as
its last row, and Chrome on Android by default opens the keyboard over the layout: `100dvh` does
not shrink, the browser pans to the focused box, and the dock — the switch with it — sits under the
keyboard. That was already true of the channel's box. The viewport now carries
`interactive-widget=resizes-content`, which has Chrome lay the page out in what is left above the
keyboard, so the dock rides directly above it with the switch in it and the list, which holds the
box, is what gets shorter. `tests/voice_composer_browser.py` checks it at 412x915 by taking a
keyboard's share of the screen away, which is exactly what that key asks of the browser; headless
Chromium has no keyboard to open. Safari does not read the key yet, so on an iPhone the keyboard
still pans.

`#43 typed-input` first shipped this composer as a row of its own in the dock, which was the
honest way to build and test the send path before there was a bar to put it in, and `#59
text-entry-button` then moved it into the control bar behind a Type toggle that converted the bar
into a text field — which is what took the switch away. Both homes are **deleted**, not left
standing beside the box: two text fields racing to be the one somebody types in is worse than
either.

**Composing pings the agent.** `{"type": "user_activity"}` is documented as resetting the turn
timeout without touching conversation content, and it is sent at most once every thirty seconds
while somebody is typing. That is the direct answer to the complaint that the agent grows impatient
during silence and starts asking whether anyone is still there: a person composing a message is
present, and this is how the connection is told so. The frame carries no content — the suite
asserts it is exactly `{"type":"user_activity"}` — so what is half-typed never leaves the browser.

**Two things about it are unverified, and are written down as unverified rather than as knowledge.**
Whether ElevenLabs echoes a typed `user_message` back as a `user_transcript` is not documented
either way. If it does, the sentence would appear twice — once because the page rendered it when it
was sent, and once when the echo arrived — so a typed turn is remembered for ten seconds and a
matching transcript inside that window is dropped. The window is a guess; settling it costs one
billed `scripts/run.sh --smoke-agent` run, whose `converse()` already sends `user_message`, and
**that run has not been made**. The second is the on-screen keyboard: the dock is a grid row of a
`100dvh` frame, so with `interactive-widget=resizes-content` the box and the dock should ride
above Chrome's on-screen keyboard on Android, but Chromium under Playwright has no soft keyboard:
the browser check simulates the resize the key asks for, and no screenshot in this repository can
prove what a phone's keyboard really does.

**Sound silences the agent's voice by receiving the audio and throwing it away**, and this is the
decision `#43 typed-input` asks to have recorded, because the alternative — renegotiating into a
text-only response mode — is indistinguishable to the reader and very different on the wire. A
text-only mode is settled at initiation: the page sends `conversation_initiation_client_data` once,
in `socket.onopen`, and reads the output format once, out of the initiation metadata. Switching
mid-call therefore means closing the socket and opening a new one, and the vendor documents no way
to resume a conversation — so the agent behind the reconnect would remember nothing, which is
exactly the context Mute exists to preserve. Dropping frames is reversible in the time it takes to
set a boolean; a reconnect is not reversible at all. The only cost is downstream bandwidth on a
socket already carrying microphone audio upstream. The suite makes that decision *checkable*:
toggling Sound must leave exactly one initiation frame on the socket.

### Mute says so out loud

`#73 mute-is-invisible`, and it came out of a real complaint: during a long mute the agent "gets
very annoying about asking 'Are you there?'", and turning the prompting down in the ElevenLabs
dashboard did not help.

**The cause is ours.** Mute withholds `user_audio_chunk` frames and does nothing else, which is the
right pause — the socket stays open, so the agent keeps its context. But it means that on the
vendor's side a muted caller and a caller who simply stopped talking are the *same bytes*, and
going quiet is exactly the condition that makes an agent check whether anyone is listening. No
dashboard setting can separate two cases that arrive identical.

So the page says it. Muting and unmuting each put one `contextual_update` client event on the
conversation socket that is already open. That is **not a new event type and not a new mechanism**:
the page already sends `conversation_initiation_client_data`, `pong`, `user_audio_chunk`,
`user_message` and `user_activity`, and it already sends `contextual_update` twice over — once
after the initiation frame for `#46 conversation-replay`, and once per relayed message for the
Discord relay. Mute is a third use of a frame that was already on the wire. It **cannot** be an MCP
tool: MCP here is request/response with the agent as the client, and this server issues no
`Mcp-Session-Id` and answers `GET`/`DELETE /mcp` with 405 precisely because it has nothing to push.
The conversation socket is the only door.

A mute engaged in the **connect window** — after the socket exists and before it opens, which is
when the control reads "Connecting…" — cannot be announced at the time, because there is nothing
open to announce it on. It is re-announced from `onopen`, or the whole call would run muted with
the agent never told: the same complaint, for the length of the conversation instead of a pause.

**Half of this is unverified, and it is the vendor's half.** `contextual_update` is believed to
inject context without consuming a turn — the same event `#46 conversation-replay` uses — but that
belief came from a recon plan rather than from the vendor's protocol reference. Two questions are
open: whether ElevenLabs accepts the frame, and whether an agent that reads it *holds* rather than
prompts. **One billed `scripts/run.sh --smoke-agent` conversation answers both** — mute for a
minute and listen — **and that run has not been made.** What is checked offline is our half:
`tests/js/voice_page.test.mjs` pins what the page puts on the wire, and `tests/elevenlabs_mock.rs`
sends the page's own sentence, read out of `web/voice.js`, to the loopback vendor and pins that it
is recognised as a contextual update mid-call, that its text reaches the agent's context, that no
turn is spent on it, and that the conversation still works afterwards.

**That last paragraph is a statement about our model, and it is only worth reading because the
model exists.** Until it did, `src/elevenlabs/mock/` had no `contextual_update` handling at all:
the frame fell into the catch-all for events the mock does not understand, so the test above passed
unchanged when the frame was renamed to `totally_made_up_event` — it pinned that unknown events are
ignored and nothing else. The mock now models the event, in one documented place, with a control
test that keeps an unrecognised event landing somewhere different. It is still our model of the
vendor's contract, not an observation of the vendor.
The fallback if the event does not exist is a short `user_message`, which is definitely in the
protocol and *does* consume a turn — the agent would say "understood, I'll wait" out loud, which is
more interruption than the prompting it replaces.

Pair it with the agent's system prompt: the starting prompt in
[`QUICKSTART.md`](QUICKSTART.md) now tells the agent, in prose, to hold — "skip your turn, say
nothing, and do not ask whether they are still there" — when it is told the microphone is muted. It
does **not** name the `skip_turn` tool, which is a separate native vendor feature described further
down; nothing here wires the two together.

**None of this makes a mute cheaper.** Billing continues while muted; the vendor discounts silent
periods but does not stop the meter. This makes a long mute quieter, not free.
### A typed conversation is a different conversation, not a call with the switches thrown

Because a text-only mode is settled *at initiation*, it can be **chosen** there — and that is what
the call's message box does when its first message is sent with nothing open (`#241
voice-composer`; it was the Type toggle's press until then). It answers a real report: reaching a text
interface used to mean starting a voice call, muting it, and silencing it. Two of
those three controls exist to manage a microphone you did not want open, and mute deliberately does
not close one — it withholds frames from a live capture graph so that unmuting keeps the agent's
context — so the phone showed the microphone as in use for a conversation that was being typed.

`start({chat: true})` therefore **skips the microphone entirely**: no `getUserMedia`, so no
permission prompt and no in-use indicator; no `AudioContext`; no capture graph; no playback. Talk
and Sound are *absent* rather than inert, because there is nothing for either to act on, and Hang
up becomes "End chat" and takes the space Talk would have had. The suite asserts the guarantee in
the only form that is actually a guarantee — `micRequests`, `tracks` and `processors` are all
empty — rather than asserting that something was muted afterwards.

The page **also** asks the vendor for a text-only response, as
`conversation_config_override.conversation.text_only`, and **does not rely on getting it**. That is
an override, and an agent whose dashboard forbids overrides ignores it silently. If audio arrives
anyway it is dropped — there is no `AudioContext` to play it with — and the fact is recorded once
in the connection details, in as many words: the override was refused, nothing is played, and the
microphone was never opened. The guarantee the page makes is the one it can keep on its own side of
the socket; the override is an optimisation on top of it, and the difference is stated rather than
blurred.

### The page has two compositions, and a capability query picks between them

The phone is the device this page is used on, so the phone layout is the one everything above
describes. On a wide screen with a pointing device it becomes something else: the transcript and
the channel are held to a **reading column** instead of filling the window, the dock follows that
column rather than spanning the desk, and a handle on the column's edge — draggable, and reachable
from the keyboard — sets how wide it is. The width is stored per browser, in characters, and is
clamped to 45–120 on the way in and on the way out, because storage is shared with everything else
on the origin and a hand-edited entry is a thing people do. The settings screen carries a slider
that drives exactly the same value, so the choice is reachable without a mouse.

**The dock is sized for a mouse on a desk, not a thumb** (`#218 desktop-dock`). On a phone it is a
strip of 44px targets above a tile of large buttons; on a desk the same controls are one wrapping
row of ordinary 36px buttons, icon and word side by side, inside the column. Everything shares one
row when it fits — about 50px of dock where the phone's composition stood 149px — and when a long
channel name and the reading buttons do not fit together, the buttons take a second row, beside the
switch, rather than the name being cut, for about 92px. The Voice/Channel switch has ONE place on a
desk, the lower right corner of the column, in every view and state: flicking it changes the row it
sits in, and a switch that moved with the row was somewhere else for the second click. The channel
and view pickers are as wide as the names they show, up to a cap; the view picker gives way first
when the row is short, and the channel's name is cut, with an ellipsis, only when the view picker is
already at its floor. Every control shows a focus ring; Tab visits the bar's controls in the order
they are seen, then the switch, then the pane's buttons — the bar's own order, as on a phone. A
reader on the newest line stays on it when the dock grows a row under them, so a call's turns are
still followed once it goes live. The phone's dock is not touched: `tests/offline_cache_browser.py`
measures it at 412px and 360px against what it was.

**Two columns on a wide desk** (`#229 desktop-two-column`). In a window at least 1000px wide, a
layout button beside the view picker puts Main in the left column and a thread in the right, as
other chat clients' desktop apps lay a channel out; the choice is kept per device. 1000px is where
each column still holds about 45 characters, the narrowest reading width the page allows; a window
narrowed below it shows one column again and keeps the choice. In two columns the view picker greys
out. The right column shows the selected thread, with its name, an X that deselects it, and its own
composer; with none selected, a card per thread, saying the summariser's `display_name: summary` or
else the thread's first message, three lines at most. The cards run as the channel's lists do
(`#261 two-column-connectors`): the newest activity at the foot, opened scrolled to it, a thread
that moves going to the foot and followed there only by a reader already at the foot, and a reader up
the list kept on their card; closing a thread comes back to the card it was chosen from. **Unread
only**, a toggle beside the column's "Threads" heading (`#259 thread-list-unread-filter`), narrows the
cards to the threads with unread replies, by the rule of the card's "N unread" or "N new" chip; it is
kept per device, works with the search, keeps the selected card where it was as it goes on and off,
and says "No unread threads." over a list it empties. The search over the cards also reads the
replies the page already holds for each thread — each term in the card's words or in its replies —
and a card found only with its replies says "in replies"; nothing is fetched for it. **Every card stays
contracted** (`#277 thread-card-focus`): a click on one, or Enter with the keys on it, highlights it —
tinted and ringed in its thread's colour and shown whole — and while it shows, every other card's edge
and unread count and every other link go grey; another card takes the highlight, and the same card
again or Esc puts it out. It is kept per channel for the session, through the mouse moving over the
cards, the keys walking them and the cards redrawn. The way into a thread is the card's own
"N replies · M unread" button, or → on the highlighted card (else the one the keys are on); ← from a
thread entered so comes back out to the cards, the keys on that card. The one-column Threads list's
cards work the same way. Over the cards,
**links** join each root on
Main to its thread's card (`#261 two-column-connectors`), as a diff tool joins a hunk on the left to
its counterpart on the right: a right-angled line in the thread's colour (its Thread(N) tag's and
the card's edge's), out of the root's right edge level with its tag, through a lane of its own in the
gap between the columns, into the card's left edge, drawn while both ends are on screen. The
selected message or card draws its link even with the other end off screen, clipped at the columns'
edge with an arrowhead pointing up or down to it (the highlighted card's, while one is highlighted,
whatever is selected), and if the mouse rests 1.5 s on the selection — on the highlighted card, while
one is highlighted, which a click on it starts — the other column glides that end level with it —
cancelled if the mouse moves on, the selection or the highlight moves or the reader scrolls that column
first; the mouse crossing other cards, and a selection or a highlight the keys or a redraw made, never
glide (`#280 scroll-stability`) — and a jump instead of a glide where the reader asks for reduced motion (the stylesheet's
`--column-glide`; the script asks no media query). The links are one SVG drawn at most once per
animation frame, from passive scroll listeners, resizes and redraws, every rectangle read before
anything is written and only what changed written; nothing is drawn or measured in one column, on a
phone, over a thread, or while the page is hidden (`#226 page-energy-profile`). Thread(N), the
unread count, N replies, the Threads screen and a card's replies button select a thread there, and a plain tap on
a message still only folds it. Search, Links and the read mode apply to both columns; Pinned stands
in for the left column's list. The poll reads Main and the selected thread (two views, one delta
each), a live reply in that thread is read once as the thread, and a refresh that brings nothing
draws nothing in either column. The thread is drawn by the same code as a thread in one column: the
right column's elements stand in for the channel pane's while the code works on that column, and
its state is swapped in around that work (`COLUMN_IDS`, `COLUMN_STATE` and `inColumn` in
`web/voice.js`). `tests/two_column_browser.py` checks the layout in Chromium at 1440x900 and 1280x800,
and `tests/thread_card_focus_browser.py` the highlight and the grey, by computed colour and by the
pixels drawn, at 1280x800 and on a 412x915 phone.

**The regime is `@media (min-width: 900px) and (pointer: fine)`, and nothing else decides it.**
Not a user-agent string — a tablet with a trackpad and a phone in desktop mode both lie to one, and
the strings keep changing — and not `matchMedia` in the script either, because two places deciding
what a desktop is, is two places that can disagree. `web/voice.js` only ever sets a number; the
stylesheet decides whether that number means anything, which is why the same code path runs on a
phone and does nothing visible there. The page suite asserts both halves: that the rules really are
inside that query, and that no rule for a capped pane exists outside it.

Judging whether the result reads as a desktop application is not something a fixture with no layout
engine can do, which is why the screenshot harness gained a second desktop width and two states for
the column at each end of its range — see "Looking at the page" below.

## The MCP endpoint

`POST /mcp` is an MCP server speaking **Streamable HTTP**. A hosted voice agent — ElevenLabs, in
the first intended deployment — is the client; this process is the server.

**Streamable HTTP, not HTTP+SSE.** MCP's original remote transport was HTTP+SSE: a long-lived
`GET /sse` stream plus a separate `POST /messages`, correlated by a server-held session. It was
superseded by Streamable HTTP, which is a single endpoint. This server implements Streamable HTTP
only, and deliberately does not offer the legacy transport rather than offering it badly — the old
one needs a session table and an always-open stream per client, which is real state on a process
that is meant to be publicly reachable.

**Stateless.** No session id is issued and none is required. Every POST carries its own bearer
token and is answered on the spot, so there is no session table to leak or fixate, and a restart
cannot strand a client. The cost is that the server cannot initiate messages — it has none to
send — so `GET /mcp` and `DELETE /mcp`, which exist in the spec for exactly that, answer `405`
rather than holding open a stream that would never carry anything.

**Content negotiation.** A client that accepts `application/json` gets a JSON body. A client that
accepts only `text/event-stream` gets a one-event SSE response and the stream ends. Both are
tested.

| Method | Behaviour |
|---|---|
| `initialize` | Echoes the client's protocol revision when it is one of `2025-06-18`, `2025-03-26`, `2024-11-05`; otherwise answers with `2025-06-18`. |
| `notifications/initialized` | Accepted, `202`, no body. |
| `ping` | `{}` |
| `tools/list` | The tools this credential may use. |
| `tools/call` | Runs one tool. |
| anything else | `-32601` method not found. Resources, prompts, sampling and logging are not implemented, and say so. |

JSON-RPC batches are refused with `-32600`. The 2025-06-18 revision removed them, and a
half-succeeded batch has no coherent HTTP status — which is not a property a write-capable
endpoint should have.

### The eight tools

| Tool | Scope | Approval intent | What it does |
|---|---|---|---|
| `recent_activity` | read | automatic | Catches up on every listed channel in one call, with no required argument: the newest message's channel and time first, then each channel's most recent lines inside the window (`hours`, default 24, at most 168) with an honest count. Listed first. |
| `list_channels` | read | automatic | Names the configured channels and which are postable. |
| `digest_channel` | read | automatic | One speakable line per recent message. |
| `read_page` | read | automatic | One step of a walk: a page that says it is a page, plus the cursor for the next one. `hours` reads from that many hours ago to now. |
| `count_messages` | read | automatic | How many, up to a cost ceiling — "at least N" when the ceiling stops it. `hours` counts from that many hours ago. |
| `find_message` | read | automatic | Describe a message in your own words, get it back in full. |
| `read_message` | read | automatic | One known message by id. |
| `post_reply` | **write** | **requires approval**, unless the owner turned it off | *Proposes* a post as the bot, and nothing is sent until the owner confirms it outside the model's tools — or, with "Ask me before the voice agent sends" off, posts it at once and says `SENT`. |

They are deliberately thin, with one exception. With a real model in the loop the model should
compose, not consume pre-chewed operations, so there is no "summarize and reply" tool. The
exception is `recent_activity` (`#190 voice-agent-tools`), and the reason is what composition
costs a VOICE agent: every tool call is another round of the speech model, and a runtime that
speaks a filler line while a tool runs may give up after a round or two that held nothing but
filler — and the listener then hears a canned "something went wrong". The requests that went that
way were the open-ended ones: "what happened in the last day" was a channel list, a clock lookup
and one read per channel. `recent_activity` answers it in one call with no required argument,
and `hours` on `read_page` and `count_messages` removes the clock lookup from the precise reads.
Each channel is still one ordinary fetch with the same noise rules and spoken times; the answer
is capped at about 8 KB, dropping the oldest lines first and saying so, and one channel that
cannot be read is named as such rather than failing the rest.

**It answers by a deadline, not when the slowest read ends** (`#210 voice-catchup-speed`). Behind
a provider bridge that runs one command at a time, three channels cost three reads in series —
five to nine seconds measured — and one stalled command blocked every tool past the voice
runtime's fifteen-second limit. So the server keeps each channel's newest window in memory
(`src/recent.rs`, apart from read-aloud's 30-second memory in `src/read_ahead.rs`): whatever the
page or a tool last read, kept current by the live feed. A
catch-up starts one read per channel — shared with a read of that channel already running, and
finished even if the caller gives up — and waits 2.5 seconds for channels it holds a copy of, 8
for channels it does not. A channel read in time is current; one that was not is answered from
the held copy and its block says `NOT CURRENT`, how old the copy is, and that it may be missing
newer messages; one with nothing held says it was not read in time and to ask again in a moment.
A window read within the last 15 seconds — 60 when a live producer feeds it — is used without a
read at all, and opening a voice session (`GET /api/v1/voice-session`) starts the reads in the
background, so the greeting covers them. The access log's `tool` line carries `millis=`, each
catch-up writes `recent_activity current= held= unread= millis=`, and a call the caller abandoned
writes `outcome="abandoned"` instead of nothing.

**No model-facing text calls this server a "bridge"** — not a tool description, not the
initialize instructions, not the channel listing. A speech runtime may have a filler call of its
own by that name, and text that also called the chat server a bridge blurred the two. That was
never shown to cause a failure, but it costs nothing to avoid, and `tests/mcp.rs` pins it.

`ask_agent` — the slow-path seam that answers HTTP 501 — is **not** offered over MCP, and cannot
be called by name either. A tool whose only possible outcome is an apology spends a model's turn
on nothing. It stays in the manifest at `GET /api/v1/agent-tools` as the record of the intended
shape.

**A read credential is not shown `post_reply` at all,** and is refused with HTTP `403` plus
JSON-RPC `-32001` if it calls it anyway. Hiding and enforcing are separate on purpose: hiding a
tool is never the thing that keeps it from running.

### Posting asks first, unless you turn that off

`#34 voice-chat-write-confirm` and `#264 voice-send-approval`. A per-tool approval setting is a
promise the voice provider makes, and a bridge that has no such setting would leave the decision to
post entirely to the model — the same model that has just read other people's messages, any of
which may be written to talk it into something. So whether `post_reply` sends is decided by this
server, from one setting the owner owns: **"Ask me before the voice agent sends"**, a checkbox in
the voice page's Settings, first in the **Voice agent** section, under **Sending**.

**The setting.** On by default. The shipped default is `[conversation] ask_before_send` in the
configuration file (`true` unless the operator says otherwise); the owner's own choice is one row
in the store, written only by a write-scope `PUT /api/v1/voice-settings` — which Settings makes,
and which no MCP tool reaches, because a model that could turn its own confirmation off could then
post whatever a message it read asked for. Anyone who can read may see it, through
`GET /api/v1/voice-settings` and client-config's `voice_settings`. With no store configured the
change is refused with 503 and the configured default stands; a store that cannot be read makes the
server **ask**, whatever the default says. A purge removes the choice and restores the default.
The checkbox says, for each state, what that means:

* **On** — "The agent reads the message back and asks 'Send it?'; say yes or tap Send."
* **Off** — "The agent sends as soon as you ask. A message it reads could also talk it into
  sending."

The server reads the setting on every `tools/list`, profile and `post_reply`, so its behaviour
changes at once; the agent's prompt changes at its next call. An agent still on the asking prompt
whose `post_reply` now posts at once is kept honest by the `SENT.` result, but **change it between
calls**, and
re-paste the prompt of a hosted agent configured by hand (see "Registering it with an ElevenLabs
agent") — unless it lets the voice page replace its prompt, in which case its next call has the new
rules (see "Instructions for the voice agent"); its `post_reply` approval at the vendor is changed
by hand either way. Settings says which, beside the switch: on a deployment whose agent is hosted
it shows a line saying what the switch does and does not reach, worded from what the last call's
agent was given.

**Off: `post_reply` posts.** It goes through `ops::reply`, the same function a confirmed proposal is
posted with — the allowlist, the write policy, the emptiness check, reply targets and the splitting
of a long message are the ones every other send gets. The model is told `SENT.` first, then where it
went and the exact text posted (and how many messages, if it was long, and where they went: the
first in the channel and the rest in the thread under it where the provider continues a long message
there — `#263 post-split-per-provider` — or all of them in the answered message's thread for a reply
where the provider threads replies, as Slack does). A refusal starts `NOT SENT.`, and one that
failed partway `PARTLY SENT. The rest was NOT sent; do not send the whole message again.`, or, when
the answer for the part that failed was lost, `PARTLY SENT. Whether the rest was posted could not be
confirmed` — in both modes, so no failure reads as a send. A failure whose answer was lost — a
timeout, a dropped connection, a gateway's 5xx, a success whose body did not parse — may have
posted, so it starts `NOT CONFIRMED.` and tells the model not to call again for the same words; it
is never called `NOT SENT`, because an owner told "not sent" says "try again". The prompt and the
tool description say the same of a call that fails with no result at all, such as a hosted tool
call that timed out.

**A direct send is posted at most once.** A commit cannot be repeated — it spends a single-use
handle — but a direct send has none, and a tool call can be retried by its transport, or asked for
again, after the post has landed. So the server remembers each direct send while it is on its way
and for a minute after it ends, and the same channel, text and reply target in that time are
answered from that memory instead of posted: `SENT already` when the first was posted (the REST
route answers it as that send did, `{sent: true}` with its serial), and `NOT CONFIRMED` or `PARTLY
SENT already` when the first may have posted or only partly did, saying, as the first send's own
failure did, whether the rest of a partly posted one was not sent or could not be confirmed (REST:
`409 post_repeated`). A send that certainly posted nothing — a refusal — is forgotten at once, so it
can always be tried again, and different words, or the same words as a reply, are a different post.
The memory is the server's own, in memory: a restart forgets it.

The log gets one content-free `post_gate event=sent_direct` line (or `direct_failed`, with the
reason, or `direct_repeat`, with what became of the earlier send): a number, the channel, the
length. There is no card and no read-back. A read credential is still refused.

**On: `post_reply` proposes.** It leaves one pending proposal on this server and answers `NOT SENT`,
with the channel, the exact text, and the one sentence to read it back with. The owner confirms
it outside anything a model can call:

* **on the voice page**, where a card shows the server's own copy — channel, reply target, text —
  with **Send** and **Don't send**; or
* **by saying yes**, which the voice page — never the model — judges, and turns into a commit
  naming `confirmed_by: "speaker_turn"` (below). A bridge may do the same under the same rules.

The card is built so that a tap confirms only what the owner read. When a newer proposal replaces
the text, Send is held for two seconds and the card says the text changed, so a tap already on its
way does not land on the new words. A draft whose card was showing when a call ended is not offered
again in the next call on the same page; it lapses on the server. The page remembers that only
until it is reloaded, so a new call from a reloaded page within the 120 seconds shows it again.

**A spoken yes, judged by the page.** The agent's tool result hands it one sentence to say,
`To {channel}: {text}. Send it?` (`ops::read_back`), and its prompt tells it to say exactly that,
adding nothing. The page takes the owner's next final transcript as a yes only when all of these
hold, for both wire protocols:

* **The draft is this call's.** The card shows a proposal this call's agent made — not one already
  pending when the call opened, not one from before a replacement voice session on the same socket
  (a second `session_started` or `conversation_initiation_metadata`), and never one from another
  call. It has not lapsed, and holds no sign-in token. Only the newest proposal is ever on the card.
* **It is a spoken call with echo cancellation on.** On in Settings when the call opened, and not
  refused by the device: a microphone track that reports `echoCancellation: false` in its settings
  counts as off, though one that reports nothing is taken at the request's word. A typed call never
  commits by voice, and neither does a yes typed into a spoken call.
* **The read-back was not interrupted.** No `interruption` (hosted protocol), no
  `turn_complete{interrupted: true}`, and on `vibe-talk-v1` the read-back's own `turn_complete`
  arrived before the answer.
* **The read-back was exactly the template.** Everything the assistant said since the last turn of
  anyone's — the owner's, spoken or typed, or a text turn of the page's own: a relayed chat message
  or a notice — or everything it said once this draft was on the card, is word for word
  `To {channel}: {text}. Send it?`, ignoring
  case and punctuation. At most one lead-in (*okay*, *ok*, *alright*, *all right*, *sure* or *got
  it*) and an optional "the" before the channel's name and "channel" after it are allowed; nothing
  else is — no inserted word, nothing between the text and the question, nothing after the
  question. So the owner has heard exactly what would be posted and where, and nothing else since
  they last spoke.
  A read-back that adds a "not", a chat message read aloud, an answer to a different question that
  happens to contain the card's words, or a re-read of new words while the old card is still up
  cannot stand in for it. Any turn of the owner's ends the read-back; only a fresh one — "read it
  back again" — or a new draft read back in full arms a yes again.
* **The draft is plain words.** The read-back is held to the card by its words, and a voice says
  only words, so anything else in a draft could change what is posted while every word still
  matched — and text-to-speech drops exactly those things: `~~don't~~ merge the PR` is heard as
  "don't merge the PR", and `github.com.evil.io` as the same words as `github.com/evil.io`. So a
  draft is confirmed by voice only when it holds nothing but letters, digits, spaces, apostrophes,
  a hyphen inside a word, a dash with a space on each side, and sentence punctuation (`. , ; : ! ?
  …`) followed by a space or the end. A mention (the model says a name there), a link or a bare
  domain, a sign such as `+1` or `#123`, an emoji, a time or a decimal (`10:30`, `3.5`), and any
  markup — strike, spoiler, quote, emphasis, code — make it tap-only, and the agent and the status
  line say why.
* **It was heard to the end.** The answer arrived after the read-back's audio finished playing. On
  `vibe-talk-v1`, which sends partial transcripts, that is when the answer began; on the hosted
  protocol, which sends only final transcripts, it is when the answer's transcript arrived, so a
  short "yeah" said during the last of the read-back that the provider did not treat as an
  interruption can still count.
* **It is not an echo.** An answer made only of the read-back's last words — a bare "send it"
  within 1.5 s of the end of "…Send it?" — may be the agent's own voice and is refused. A plain
  "yes" is not among those words unless the card's text ends with it.
* **It is the very next answer, and a yes.** A yes is a phrase from a closed list — *yes, yeah, yep,
  send it, send, do it, go ahead, sure, please do* — with punctuation and case ignored and at most
  one word of filler ("yes please", "ok send it"). A yes inside a sentence that adds anything
  ("yes, and tell him I'm late", "send it to Bob") sends nothing, nor does a question ("Send it?").

**What came before the answer, on `vibe-talk-v1`** (`#268 voice-call-drops`). A bridge relays the
owner's words and the assistant's as two streams, and the answer's final transcript can reach the
page after the assistant's reply to it has begun: "One moment." arrived before the "Yes." it
answered, and the page used to hold that reply up as part of the read-back, so an exact read-back
and a plain yes sent nothing. The rules above are now asked of what was said BEFORE the answer:

* An assistant turn numbered the same as the answer, or later, whose words are exactly the
  acknowledgement the agent is told to give a yes — "One moment.", case and punctuation aside, or
  its first word "One" while that turn is still streaming — is taken for the answer's reply and is
  no part of the read-back. Anything else such a turn says ("Or should I cancel it instead?", "One
  moment, or should I cancel it?") stays part of the read-back, so the yes is refused; and if such
  a turn completes interrupted, the read-back counts as interrupted. A turn taken for the reply
  that goes on to say more, or that completes on "One" alone, stops a yes that is still settling;
  so does a turn numbered the same as the answer or later that first appears while the yes is
  settling ("Actually, should I cancel it instead?") unless it is the acknowledgement, which is
  then held to the same rule. A `turn_complete` that names no turn may have completed any such
  reply, so each is then judged as completed: one still on "One" stops the yes.
* That rests on an **assumption the protocol does not state**: that `vibe-talk-v1` numbers the
  owner's words and the assistant's answer to them alike. The safety of the rule does not depend on
  it. Only the acknowledgement is ever left out, whichever side of the answer it was really said
  on, so if the assumption is wrong the most it lets through is a yes whose exact, completed,
  heard-out read-back was followed by "One moment." before the owner answered — or by a "One"
  whose turn was still streaming, with no completion of any kind, when the settle window ran out,
  since what such a turn goes on to be can stop the yes only while it is settling. Without turn
  numbers nothing tells a reply from more of the read-back (the order of arrival is what cannot be
  trusted), so a provider that sends none is judged as before, over everything said since the
  owner last spoke.
* "Not interrupted" and "completed before the answer" are asked of the read-back's own numbered
  turn: its `turn_complete`, not interrupted.
* "Heard to the end" compares the answer with the moment the read-back's audio finished playing —
  the audio queued when its `turn_complete` arrived, which the server sends after the last of it —
  not with the reply that may already be playing. The answer began at its first frame, or at its
  reply's first frame if that came sooner, which is never more lenient.
* An answer the bridge streams only as hypotheses, never marking one final, is judged when its turn
  completes, with the words the page shows and stores for it then.
* A final transcript of an older turn that arrives after the owner's next turn has begun ("Yes",
  then "wait, no", and only then "Yes." marked final) is not the answer: it sends nothing, is
  answered as more than a yes, and the newer turn is judged when it ends.
* "Everything it said once this draft was on the card" is counted in words within a turn as well
  as by turns, for a turn that began with no draft on the card: "Sure, let me draft that." before
  the tool call and the read-back after it can be one numbered turn, which began before the card
  did. A draft that replaced another in the middle of a turn gets no such allowance: a turn that
  read the first draft back, saw it replaced and read the replacement does not confirm the
  replacement, as before.

The read-back must still be exact, plain words, uninterrupted and heard out, the answer the very
next one and a plain yes, and the post goes at most once. Besides the bridge's order and an answer
that never got a final transcript, two things are accepted that were not before, both stated above:
a read-back after words the agent said in the same turn before the first draft appeared, which is
what "once this draft was on the card" always said; and, if the stated assumption is wrong, "One
moment." between the read-back and the yes.

A yes that passes waits 1.2 s, so a "wait" transcribed within that time sends nothing, then
commits exactly the card's server copy through the same function and busy latch as a tap: a tap and
a yes racing commit once, and the single-use handle is the backstop. On the hosted protocol a second
transcript usually arrives only after the provider's end-of-turn silence, which is often longer
than that, so "yes… wait" is not a reliable way to stop it there: say no, or tap **Don't send**,
before you say yes. An answer that starts *no, nope, cancel, stop, wait, don't, do not* or *never
mind*, given while sending is the subject, withdraws this call's draft. Anything else leaves the
card for a tap.

**Every yes to sending is answered, and every outcome is said, three ways.** On a yes the agent
says only "One moment." and waits for the page; it is never told to say "Sending.", which would be a
claim before the fact — and it will take far more than the closed list for a yes. So while sending
is the subject — any question the assistant asked since the owner's last turn was about sending
("Send it?", "Shall I post that?", even with a relayed message read out after it that asks a
question of its own), one of its turns ended on a read-back's "send it" whatever the punctuation,
or the answer itself says send ("yep that's right, send it", "ok send it" after a digression) — the
page answers every answer an agent could take for a yes: a plain yes; anything with a word like
one (*sounds good, go for it, uh-huh, mm-hmm, perfect, alright, that's right, of course, ship it,
affirmative…*); and anything of three words or fewer that does not start like a no (*make it so,
fire away*). It answers SENT, or NOT sent and why, while this call's draft is on the card; and if
the draft has gone — it lapsed and the card was taken down, or it was settled — it says no draft is
waiting and nothing was sent. A typed answer like a yes is answered the same way. A yes to some
other question ("want to hear the rest?") is not waited on, so it changes only the status line;
answering every "okay" would have the agent repeat itself to each acknowledgement. Nor is a longer
answer with nothing like a yes in it ("change it to Friday"): that is a request of its own, which
the agent acts on. In a call that began with "Ask me before the voice agent sends" off, the agent
was told to post at once and asks nothing, so the page judges no answer at all. The agent is told
through the call's existing text channel — the `user_message` the composer and the live relay
already send, which `vibe-talk-v1` carries as a queued `prompt` once the spoken turn has closed
(`#241 voice-composer`) — in one fixed sentence starting `Voice page:`: SENT, partly sent, NOT sent
with the reason, not confirmed, or no draft waiting. The notice for a yes the page did not take
ends "Otherwise carry on", so a yes that was in fact about
something else does not derail the conversation. The prompt tells the agent never to say "sent"
before that notice, so it never claims a send that did not happen, and its context stays truthful
for a later "did it go?". The page also says it itself, because a model's sentence needs a
cooperative model: the status line ("Sent to lead team." / "Not sent — the read-back was
interrupted."), a transcript seam, and a short tone queued after the speech already playing — a
rising pair for sent, one low note for not sent; none when Sound is off or in a typed call. A tap on
**Send** or **Don't send** during a call is told to the agent too (the screen already says it), so
an agent waiting on "Send it?" hears the result.

**What a spoken yes cannot know**, stated plainly:

* **Who spoke.** The page assumes one speaker. A passenger, a phone call on the car speakers or the
  radio saying "yeah" right after a read-back counts as the owner.
* **Background speech** is narrowed — the next-answer rule, the closed list, the settle window —
  not eliminated.
* **Misrecognition.** Short words are the commonest false transcripts, and the page has no
  confidence score to consult.
* **The audio itself.** The read-back is checked against the provider's text of the agent's turn,
  which is what its voice speaks; the page does not listen to the audio.
* **Echo.** It needs echo cancellation on; say a plain "yes" rather than "send it" right after the
  question.
* **Which message a reply answers.** The read-back names the channel, not the message a reply is
  attached to; the card shows that.
* **A yes to no question.** The page answers a yes only when sending was the subject: a question
  about sending since the owner's last turn, a turn ending on "send it", or the answer itself. An
  agent that ignored its prompt — asked nothing about sending ("Should I go ahead?"), or asked
  nothing at all, then took a bare "okay" as agreement and said "One moment." — hears nothing back
  from the page; the status line still says it was not sent.
* **What a no meant.** Any answer that starts like a no withdraws the draft while sending is the
  subject — "no problem, send it" included, and a "stop" said only to stop the agent talking. That
  is the safe direction; ask for the draft again.
* **Coverage.** Only plain words can be confirmed by voice (above). A draft with a mention, a
  link, a symbol, an emoji, a time or a decimal, or any formatting falls back to a tap, and so does
  one whose words the model does not read back exactly as written; the agent and the status line
  say which.

The mechanics of a proposal, all over the write-scope REST routes in the table above:

* **One proposal at a time.** A new one supersedes the old, so a correction ("no, say tomorrow")
  replaces the draft rather than queueing a second post.
* **The handle is a capability.** 128 random bits, returned only by `GET /api/v1/post-proposals`.
  It is never in the tool's answer to the model, the answer of the REST route that proposes, a path,
  or a log line.
* **Single use, short-lived.** It lapses 120 seconds after the proposal. Every commit that reaches
  the gate with a pending handle spends it — one that restates the wrong text burns it rather than
  inviting a retry, and so does a confirmed post that then fails to send. A request too malformed to
  parse (a missing field, an unknown `confirmed_by`) is refused before it reaches the gate, and
  spends nothing.
* **Bound to what was read back.** A commit restates the channel, the text, and the reply target,
  and must match all three exactly.
* **Refusals are specific:** `proposal_unknown` (404), `proposal_expired` (410), and
  `proposal_used`, `proposal_superseded` or `proposal_mismatch` (409). None of them posts.
* **Scope is checked before the request is read**, here as on every `/api/` route (see
  "Security"): a read token gets 403 and no token gets 401 whatever body, content type, query or
  channel id it sends, so the parsers tell a caller without write scope nothing about what they
  accept.
* **The log is content-free.** One `post_gate` line per step — `proposed`, `committed`, `failed`,
  `refused`, `cancelled`, `superseded`, `expired`, and `sent_direct` / `direct_failed` /
  `direct_repeat` for a direct send — with a serial, channel id, text length, refusal code and
  `confirmed_by`. Never a word of
  the text, never the handle. Changing the setting logs `voice_settings event="changed"` and the
  boolean alone.

**A bridge that commits on a spoken yes must keep the same rules.** Two cases in particular, which
the page's rules above already cover:

* **An interrupted read-back.** A barge-in can cut the response after `post_reply` has run but
  before the text was spoken. The proposal is still pending, and the next user turn may be "stop"
  or something unrelated, not agreement to text the owner never heard. Commit only after a
  read-back that finished normally, following the proposal; otherwise cancel it, or let it lapse.
* **A replacement model session.** A bridge that reopens a session after a cut and seeds it with
  earlier user turns only gives the new model no knowledge that a proposal is pending, so a yes
  reaching it has no context at all.

**The gate binds the model, not the write credential.** `confirmed_by` is recorded, not verified:
any holder of the write token can commit, naming either path, and `POST /api/v1/channels/{id}/reply`
still posts directly for the application callers that use it. That is the point of the design —
the model's tools reach only the proposing half — and also its limit: it holds only while nothing
model-driven can make arbitrary HTTP calls with the write token. A voice bridge is given write scope
only on those terms.

The read-scope tool list is unchanged by this: a read credential still sees the six read tools and
nothing that proposes, commits, or cancels.

**A channel may be named, but the id is what the model is told to pass.** Every tool's
`channel_id` is described as "an id from list_channels" and lists the configured choices
(`post_reply` lists only the writable ones). A model that passes the name it heard anyway —
the configured label or the operator's alias, matched case-insensitively — reaches that channel;
a name two channels share reaches neither. Naming only picks from the allowlist and never widens
it: a read-only channel named by its label is still refused for posting. An `unknown_channel`
result lists the ids to use instead, so a wrong guess costs one retry rather than the answer.

**An id may arrive as a JSON integer.** The schemas say `string`, but a caller that sends
`channel_id`, `message_id`, `before` or `reply_to` as an exact unsigned integer means the id
with those digits, and gets it. Anything else in that place — a float, a negative number,
`null`, an array — names nothing: a float has already lost digits, and rounding it would pick a
different message rather than none. What this cannot catch is an integer some earlier hop
already rounded through a float and wrote back out as a whole number: it arrives looking exact
and is read as written. A channel id rounded that way falls outside the allowlist, but a
rounded `reply_to` is handed to the provider as it is — so a caller that can, should send ids
as strings. The `tool` access line's `channel` field says which shape
an unreadable id arrived in (`<unreadable number>`, `<null>`, …), and `<string arguments>` when
`arguments` itself was a string; `-` still means an object with no `channel_id` at all.

**Every message a tool renders carries its author's mention token**, written as
`[id | time | author <@author id>]`, so a model that wants to notify someone copies a working
token instead of assembling one — and can only ever mention someone who has posted in an
allowlisted channel. See "The API" above for why there is no lookup tool.

**Every read tool's text output is fenced.** Channel content comes back inside the
`src/untrusted.rs` fence, with the data-not-instructions notice attached, with any forged fence
marked (not deleted) and control characters stripped. That is what the model actually receives.

### Registering it with an ElevenLabs agent

**Before anything else, check one blocker.** MCP integrations are **unavailable on ElevenLabs
workspaces in Zero Retention Mode, and on HIPAA-enabled workspaces.** On those the custom-MCP-server
option is not offered at all, and this entire integration path is closed — there is no workaround
here. Check the workspace setting before spending an hour on the rest.

Create a fresh agent and keep ElevenLabs' hosted LLM; nothing in this project needs you to bring
your own model. Then add a custom MCP server integration:

| Field | Value |
|---|---|
| Server URL | `https://<your-public-hostname>/mcp` — the path **must** be `/mcp` |
| Transport | **Streamable HTTP** |
| Authentication | header `Authorization`, value `Bearer <your vibe-talk read token>` |
| Approval mode | **Fine-Grained Tool Approval** |

**Streamable HTTP, not SSE.** If the dashboard also offers SSE, do not pick it. HTTP+SSE was MCP's
original remote transport and was superseded by Streamable HTTP in protocol revision `2025-03-26`;
this server implements only the current one, so choosing SSE will simply fail to connect.

Then set per-tool approval. This table is the whole policy:

| Tool | Approval setting | Why |
|---|---|---|
| `list_channels` | **auto** — no approval | Names the configured channels. Touches Discord not at all. |
| `digest_channel` | **auto** — no approval | One spoken line per recent message. The main one. |
| `find_message` | **auto** — no approval | A description in your own words → that message, in full. |
| `read_message` | **auto** — no approval | One known message by id. |
| **`post_reply`** | **REQUIRE APPROVAL** | **Speaks in your name, in your channel.** |

That is what implements "reading is automatic while driving, posting asks first". If the agent
only shows four tools, you gave it the read token — which is correct, deliberate, and means
posting is off entirely rather than gated.

A starting system prompt for the agent — triage rather than transcription, which is the whole
reason a digest exists — is in [`QUICKSTART.md`](QUICKSTART.md) under step 5.

**Which token you give it decides the ceiling.** With the read token the agent physically cannot
post — `post_reply` is not even listed for it. With the write token it can *propose*, and a
proposal posts only when the owner confirms it — see "Posting asks first, unless you turn that
off". The conservative first deployment is the read token.

**The prompt and the approval follow the owner's setting.** With "Ask me before the voice agent
sends" on, paste the `write-confirm` prompt and keep `post_reply` on **REQUIRE APPROVAL**. With it
off, paste the `write-direct` prompt (from `GET /api/v1/voice-agent` with the write token) and set
`post_reply` to **auto**: the owner turned confirmation off, and a vendor prompt in front of every
send would be the friction he removed. **A hand-configured agent does not follow a change by
itself — re-paste both whenever the setting changes** — unless its security settings let a client
override its system prompt: the voice page then hands it the prompt for the setting as every call
opens (see "Instructions for the voice agent"), and only the approval is still changed by hand.

**Say this plainly: the per-tool approval setting is enforced on ElevenLabs' side, and this server
cannot verify it.** Nothing here can tell whether you configured approval correctly, or
whether it was later changed. What this server enforces is the scope split, the allowlist, and the
confirmation step: a read token cannot post, `post_reply` never sends by itself while the owner has
the agent ask first, and no token can reach a channel outside the configuration. Do not mistake the approval prompt for a guarantee we
implement; the confirmation step is the one we do.

Verify the public endpoint before pointing an agent at it — same script, public URL:

```sh
scripts/verify-deployment.sh --url https://<your-public-hostname> --channel <snowflake>
```

The read token's `tools/list` must NOT contain `post_reply`; check 2 is the one that asserts it.
If it fails there, something is wrong with the deployment, not with the agent, and no amount of
dashboard configuration will fix it.

## Exposing it: one worked example

**This is optional, and this vendor is not special.** See **From zero → Expose it** above for what
is actually required (a URL a browser can load; HTTPS if you want the microphone) and for the
other ways to get there — an SSH reverse tunnel, a reverse proxy with a certificate, a cloud load
balancer, an overlay network, or no exposure at all. One example is written out in full here
because a complete recipe is worth more than six sketches; pick a different one freely.

What every route shares: the server speaks plain HTTP and holds a bot token, so it must never be
reachable directly from the internet. Something in front of it terminates TLS.

The example is a Cloudflare Tunnel, which gives it a public HTTPS hostname with **no inbound port
open** on the host: the `cloudflared` daemon dials out and Cloudflare terminates TLS. You need a
domain on Cloudflare; the hostname can be a subdomain of one you already have.

```sh
# once, on the host
cloudflared tunnel login                                     # browser; pick the zone
cloudflared tunnel create vibe-talk                          # prints a UUID and a JSON path
cloudflared tunnel route dns vibe-talk vibe-talk.<your-domain>
```

`tunnel create` prints the two values the next file needs: the **tunnel UUID** and the path to the
**credentials JSON** it just wrote (normally `~/.cloudflared/<uuid>.json`).

`~/.cloudflared/config.yml`:

```yaml
tunnel: 6f2a1c30-1111-2222-3333-444455556666
credentials-file: /home/<you>/.cloudflared/6f2a1c30-1111-2222-3333-444455556666.json

ingress:
  - hostname: vibe-talk.<your-domain>
    service: http://127.0.0.1:8080
  # Required, and must be last: cloudflared refuses to start without a catch-all rule.
  - service: http_status:404
```

`tunnel:` also accepts the tunnel's name, but the UUID is what `create` handed you and it cannot be
ambiguous.

```sh
cloudflared tunnel run vibe-talk        # foreground first, so you can watch it connect
```

Verify the public hostname with the same deployment check you ran locally:

```sh
scripts/verify-deployment.sh --url https://vibe-talk.<your-domain> --channel <snowflake>
```

Then install it so it survives a reboot:

```sh
sudo cloudflared service install
sudo systemctl status cloudflared
```

Bind the server to loopback when you do this — `VIBE_TALK_BIND=127.0.0.1:8080` — so the only path
in is the tunnel. With Podman, publish to loopback: `-p 127.0.0.1:8080:8080`. That last rule is
the one part of this section that transfers unchanged to every other way in.

### If you chose Cloudflare: do not put Cloudflare Access in front of this

Access is the obvious next fence and it is the wrong one here. **Access expects a human with a
browser**: an unauthenticated request gets a login redirect. ElevenLabs calls this endpoint
machine-to-machine, with no browser and nobody to log in, so Access bounces every call — and the
failure shows up inside ElevenLabs as a vague connection error rather than as "there is an
identity gate in the way", which is an expensive hour.

If you want Access anyway it has to be a **service token**: create one under Zero Trust → Access →
Service Auth, write a policy allowing it on this hostname, and add both headers to the ElevenLabs
MCP server configuration alongside `Authorization`:

```
CF-Access-Client-Id:     <client id>.access
CF-Access-Client-Secret: <client secret>
```

**The general rule, whichever way in you chose: no browser-shaped identity gate in front of
`/mcp`.** Any SSO, OAuth login wall, or "sign in to continue" interstitial — Cloudflare Access,
an nginx `auth_request` against an IdP, an application load balancer's OIDC action — assumes a
human who can be redirected. A hosted voice agent cannot be. If you want a second fence there, it
has to be one that a machine can satisfy with a header it was configured with.

**Transport is not authorization.** A tunnel, a proxy or a load balancer gives you TLS and hides
the host's address; none of them decides who may call. This server's bearer tokens do that, and
that is the part this codebase tests.

## Where ElevenLabs attaches

From the related-work review, and recorded in `src/mcp/mod.rs` so it does not have to be rediscovered:

* ElevenLabs Agents connect to **remote MCP servers over SSE or streamable HTTP** — so this server
  is the MCP endpoint and the agent is the client.
* Auth is a **secret token or custom headers**, which is why this server's API is bearer-token
  based: the agent configuration carries the read token.
* There are **three approval modes**, and the useful one is **per-tool approval**, which maps onto
  the rule this project wants: **reading is automatic, posting asks first.**
* **Barge-in and `skip_turn` are native**, but neither one is pause. Barge-in interrupts the
  agent while it is *speaking*; `skip_turn` is the agent choosing to hold. In both cases the
  socket stays open and the microphone keeps streaming. There is no vendor pause primitive: the
  only transport-level action is closing the conversation. **Pause is ours** — muting stops
  uploading audio while keeping the socket and the agent's context (`web/voice.js`), and Sound off
  silences the agent's voice while its replies keep arriving as text.
  Two things follow, and both are observations from use rather than claims of ours: **billing continues
  while muted** (a conversation is billed for being open, though the vendor discounts silent
  periods), and **the agent will start asking whether you are still there**, because a
  client-side mute is invisible to it — from the vendor's side, muted and "went quiet" are the
  same thing. See *Mute says so out loud* above for what the page does about the second one, and
  for the part of it that is still unverified. The first one has no fix: a muted call is a call.
* Caveats: MCP is unavailable on Zero Retention Mode **and HIPAA-enabled** workspaces — which
  would block this integration outright — channel text transits ElevenLabs, and conversation
  costs roughly $0.01/minute.

`GET /api/v1/agent-tools` serves the tool list with that policy attached, and a test asserts the
invariant that **every mutating tool requires approval and every read-only tool does not**. The
live MCP endpoint builds its `tools/list` answer from that same manifest, so the tools a model
sees and the tools this project documents cannot drift apart.

### Why this server speaks MCP itself, rather than wrapping something that already does

There is an existing Discord MCP server on hand: the `discord` plugin shipped in the Claude Code
plugin cache under Apache-2.0 (publisher not stated in its metadata — no `author`, `repository`,
or `homepage` field, and no marketplace manifest naming one). It is a real MCP server with
Discord fetch/reply/react/edit/download tools and a thought-through access-control layer. Three
options were weighed.

**Wrap it with a stdio-to-HTTP proxy.** `mcp-proxy` is the obvious candidate and it is ruled out
on its own documentation, on two counts. Its mode that exposes a local stdio server remotely is
**SSE only** — Streamable HTTP appears only in the opposite direction, as a client connecting to
a remote server. And it has **no inbound authentication**: its `--headers Authorization` flag
forwards a token *outbound* to a backend; there is no mechanism to *require* one from callers.
Wrapping as-is would therefore leave an unauthenticated public endpoint that can post to your
Discord. That is disqualifying, and it means "wrapping" would really mean writing both a
Streamable HTTP server transport and the authentication into a third-party codebase — with the
security-critical half written by us anyway.

**Have this server proxy to the plugin as a stdio subprocess.** This keeps the plugin unmodified,
which is the appealing part. It was rejected because the policy would still have to be
re-implemented on top of it: the plugin's tools are keyed on *its* allowlist
(`~/.claude/channels/discord/access.json`, a pairing model designed for one human DMing an
assistant), not on this server's channel allowlist and read/write split, so every call would have
to be intercepted and rewritten — policy enforced on a surface we do not control, with two
allowlists free to drift apart. It also brings a Bun runtime, a supervised subprocess, a
persistent gateway connection and pairing state into a container that currently has one static
binary and no state. And the "upstream updates keep applying" benefit is thinner than it looks:
the plugin is a pinned `0.0.4` cache directory with no repository URL, updated by the Claude Code
plugin mechanism rather than by anything a container can track.

**Extend this server — chosen.** It already had every property the endpoint needs: bearer tokens,
a read/write scope split, a channel allowlist a valid token cannot escape, secret redaction, the
untrusted-content boundary, a Containerfile, and a passing test suite. What was missing was a
protocol, which is roughly 500 lines of JSON-RPC over a `Discord` trait that already does the two
things v0 needs. One process, one language, no subprocess to supervise, and the security
properties are enforced where they were already tested.

The plugin remains valuable as a **reference**, and was read as one. Its most useful specific:
**Message Content Intent must be enabled in the Discord Developer Portal**, or the bot sees empty
message content.

## Security

This is the part that deserves the care. The server holds a credential that can read and post to
your channels, and it is designed on the assumption that it may become publicly reachable — which
makes it, not the voice agent, the real security boundary of the whole design.

**What v0 does.**

* No unauthenticated route touches Discord. Every `/api/` route **and the `/mcp` endpoint**
  requires a bearer token, checked with a non-short-circuiting comparison. `/mcp` authenticates
  *before* it parses the body, and answers a bad or missing credential with a fixed
  `{"error":"unauthorized"}` — a test asserts that body names no tool, no channel, no protocol
  revision, and not even the service. An unauthenticated caller learns only that something is
  listening.
* **Every `/api/` route checks the credential before it reads the request.** The scope — or, on
  the two read-aloud playback routes, the ticket in the path — is settled before the path
  parameters, query or body are parsed, so a caller without it gets the same `401`, `403` or `404`
  whatever path parameters, query or body it sends, and the parsers tell it nothing about what the
  route accepts. A caller who holds the credential is still told exactly what is wrong with a
  malformed request. `tests/scope_first.rs` keeps an inventory of every route, checked against the
  router's source and against the methods the router actually answers, so a new route cannot
  quietly skip this. Routing does not answer first either: without a token, an unknown `/api/`
  path or an unserved method gets the same `401` as a real route, not a `404` or a `405` that
  would map the API. With a valid token, routing answers exactly as before.
* **Reading and posting use different tokens.** The token you put on your phone and in the voice
  agent cannot post. The server refuses to start if the two tokens are equal or shorter than 24
  characters. Over MCP the read token is not even shown `post_reply` in `tools/list`, and is
  refused with `403` if it calls it by name anyway.
* **The model cannot post by itself — unless the owner says it may.** With the write token and
  "Ask me before the voice agent sends" on (the default), `post_reply` only proposes; the post
  happens when the owner confirms it through a route no MCP tool reaches, by a tap or by a yes the
  voice page judges. With it off, `post_reply` posts when asked, and a message the agent reads could
  talk it into posting; that is the owner's choice to make, the setting says so, and no tool can
  make it for him. See "Posting asks first, unless you turn that off".
* **Configured channels are an allowlist.** A channel absent from the configuration answers 404
  even to the write token, however many channels the bot happens to be in. Each channel is
  additionally `writable` or not, defaulting to not. **Both front doors share one implementation**
  of that rule (`src/ops.rs`), and a test drives the same unconfigured snowflake through the REST
  route and the MCP tool and requires both to refuse — so the two cannot drift.
* **Posts cannot ping.** Every outgoing message sets `allowed_mentions: {parse: []}`, so a repeated
  `@everyone` in a summary cannot notify a server.
* Empty and over-long posts are refused before Discord sees them.
* Secrets are wrapped in a `Secret` type whose `Debug` prints `<redacted>`; a test asserts that
  neither it nor the whole `Config` leaks a token when formatted.
* The image runs as a non-root user and contains no configuration.
* Durable state is `0600` in a `0700` directory, is bounded by retention, and is off unless an
  absolute path is configured. See **Durable state** above.
* **Channel text becomes markup in one place, on the server, through a sanitizer.** Message bodies
  are rendered by comrak and cut by ammonia to a short allowlist — paragraphs, lists, emphasis,
  code, quotes, four heading levels, tables, a mention chip, and links to `http`/`https` only,
  each opening in a new tab with `noopener noreferrer nofollow`, and with a `title` only when it is
  the address of a link drawn with a short label; no script, style, event handler, `src`, `id` or
  `class` beyond the few listed. Raw HTML written in a message is shown as text.
  The page parses exactly one kind of string as HTML, a message's `content_html`, through an inert
  `<template>`; everything else it shows, message text included, goes in through `textContent`.
  The page suite holds that sink to one call site. Only this server writes `content_html`: a
  live-event adapter's copy is discarded on arrival. See [How message text is drawn](#how-message-text-is-drawn).
* **One message cannot make the server render for long.** Length, table cells, nesting depth and
  HTML growth are each bounded before or while a body is rendered, and a body past a bound is sent
  as plain text; see [How message text is drawn](#how-message-text-is-drawn).

**What v0 does NOT do — the honest list.**

* **No TLS.** Bearer tokens over plain HTTP are readable in transit. Terminate TLS in front of it
  before it is reachable from anywhere but a LAN.
* **No rate limiting and no brute-force delay.** A 33-byte random token is not guessable in
  practice, but nothing here slows an attacker down or tells you they tried.
* **No audit log of posts.** Posting is logged only as ordinary server output. If the bridge ever
  posts something you did not authorize, there is no separate record to reconstruct it from.
* **No token rotation.** Changing a token means restarting the server.
* **One token per scope, not per client.** Every reader shares one credential, so revoking one
  reader revokes all of them.
* **The write token is a full posting capability.** Anything holding it — the voice agent, a
  phone's local storage, a shell history, a container inspect — can post as the bot. Treat it as
  you would the bot token itself.
* **Approval is advisory in v0.** "Posting asks first" is enforced by the *agent platform's*
  per-tool approval setting, which this server describes but cannot verify. The server-side
  enforcement is the scope split and the allowlist, nothing more. Do not give the write token to
  anything you would not let post unsupervised.
* **No per-request replay protection, and no nonce.** A bearer token captured anywhere it is
  stored — the ElevenLabs agent configuration, a phone, a shell history, `podman inspect` — can
  be replayed until the token is changed, which means a restart.
* **The MCP endpoint has no request size limit of its own** beyond axum's defaults, and no
  concurrency cap. A caller holding a valid token can make the server fetch from Discord as fast
  as it will answer.
* **Channel text transits the agent platform.** Anything `digest_channel`, `find_message` or
  `read_message` returns is sent to ElevenLabs and to whatever model is behind it. The fencing
  constrains how it is *framed*, not where it goes.
* **Stored transcripts are not encrypted at rest.** Once `storage.path` is set, your own
  speech and the channel text the agent read aloud sit in the clear in a SQLite file that
  outlives the container. Anyone with host filesystem access can read it. Retention bounds how
  much accumulates; it does not protect what is there.
* **The bot's own permissions are the real ceiling.** Give the bot the narrowest Discord
  permissions that work, in the fewest channels. Server-side allowlisting is a second fence, not
  the first one.

**Prompt injection.** Discord message content is written by third parties — other people, other
teams' bots, anything that can post a webhook. It is **data, never instructions**. `src/untrusted.rs`
holds that boundary: content handed to a model is fenced, and any attempt to forge the fence is
neutralized *without deleting the hostile text* (deleting it would hide the attempt). Control
characters are stripped so escape sequences cannot smuggle framing. **Every MCP read tool returns
its channel text inside that fence** — the MCP path is the one where text reaches a model without
a human in between, so it is the path that most needs it, and an end-to-end test seeds a message
carrying a forged fence and a fake `SYSTEM:` directive and asserts the fence survives, the
forgery is marked, and the hostile text is still readable as data. What code cannot enforce is
that a model obeys the framing — which is precisely why posting is a separate, differently
credentialed capability instead of something a summary can trigger. Every read response also
carries an `untrusted_content_notice` field restating this to whatever consumes it.

## Development

```sh
cd vibe-talk
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

# the deployment check, against an in-memory Discord — no bot token needed
cargo run -- --config vibe-talk.toml --fake-discord &
scripts/verify-deployment.sh --url http://127.0.0.1:8080 --channel <a-configured-snowflake>
```

315 Rust tests plus a 159-test suite for the `/voice` page: unit tests beside each module,
end-to-end tests in `tests/api.rs` that drive the real router against the in-memory Discord, and
`tests/mcp.rs` doing the same for the MCP endpoint. `tests/elevenlabs_mock.rs` is the one place
the WHOLE chain runs — the real `HttpElevenLabsClient` mints against a loopback ElevenLabs
substitute, a real WebSocket is opened to it, and the conversation drives real MCP `tools/call`s
into the in-memory Discord, so the answer the agent speaks is made out of channel text that really
came back. The scenario that answers WITHOUT calling any tool — the 2026-08-19 production failure
— is one of its cases, offline and free. The page suite
(`tests/js/voice_page.test.mjs`) executes `web/voice.js` itself against a small strict DOM whose
element set is read out of `web/voice.html`, so a script reaching for an element the page does not
have fails there rather than silently at the roadside; `cargo test` runs it through
`tests/voice_page.rs`, which FAILS rather than skips when Node is absent. The fake is not a rubber stamp — it shares the real client's
request validation and ordering contract, and it records what was posted, so the API tests assert
the actual channel, content, and reply target that reached it. A handler that dropped a post,
posted to the wrong channel, or ignored the scope split fails those tests. The parts of the live
Discord client that cannot be exercised without a token — the URL, the `Bot ` authorization prefix,
the request body, and the payload-to-message mapping — are pure functions with their own tests.

**The page suite's scroll position is clamped, exactly as a browser clamps it**, and that is not
tidiness. `scrollTop` in a browser can never exceed `scrollHeight - clientHeight`; a fixture that
took `scrollTop = scrollHeight` at its word — which is how the page pins itself to the newest line
— modelled "at the newest message" as a whole viewport PAST the last row, so every question the
suite asks about following the newest line was being answered about a position that cannot exist.
It also left the anchoring assertion weaker than it reads: `getBoundingClientRect().top` is
`offsetTop - scrollTop` here and the restore is `scrollTop += top - before`, so "the reader did not
move" is algebraically forced for any height change at all — including a restore that scrolls past
the end of the content. The clamp refuses that, the anchoring test additionally requires the
resulting position to be strictly inside the range, and it carries a second negative control: the
same formula applied against a DIFFERENT element has to fail. `#74 scroll-test-strength`.

The safety tests were checked against deliberate breakage rather than assumed to have teeth. Four
mutations were applied one at a time and the suite was required to go red for each: removing the
bearer check in the MCP transport (3 failures), removing the channel allowlist in `src/ops.rs`
(3), removing the fence-forgery neutralization in `src/untrusted.rs` (5), and removing the
write-scope fence in the MCP dispatcher (1). All four were reverted; the suite is green.

### Looking at the page

Every test above drives behaviour. **Not one of them lays anything out**, so not one of them can
tell you the page looks right — the `/voice` suite says so about itself. That gap is not
theoretical: a single photograph of a real phone showed three defects the whole suite had
passed over, because a clipped paragraph and a control that reads as active on a dead call are
layout facts and a fixture with no layout engine has no opinion about them.

```sh
scripts/run.sh --screenshots
```

Photographs the `/voice` page in the thirty-six states that look different — signed out, idle, live
call, muted, the agent's voice silenced, just after a hang-up, the end-of-call seam with its
disclosure open, the clear control armed, settings, the Discord view, a long transcript parked
mid-scroll, that same list with one folded answer opened among the closed ones, the moment a turn
arrives while the reader is up in the history, the desktop reading column at each end of its
range, and the two connection outcomes that used to look identical — a call suspended by the
phone, and one that really failed — the reply screen with a short target and with one longer
than the frame, one channel row picked out under the pointer, a step further back through the
channel, the earlier conversation restored from the server after a reload, a turn that was typed
rather than spoken, the control bar in each of its two homes, that same bar converted into a
text field, the bar packed with every button it has, a channel message that arrived because the
SERVER pushed it rather than because the page asked, a resumed call whose reconstruction was
only partial and says so, the channel picker on the bar with the history walked back several
pages, the channel pulled down past the point where letting go refreshes it, the channel with its
collapsed rows summarised instead of clipped, the channel filtered to what has not been dealt
with, the bulk clear saying how many it is about to take, the channel filtered by a quoted phrase
typed into the search glass, and the same summary mode when the
summariser FAILS — the row red, saying so in words, with the message still under it — at four
viewports: a tall phone, a
short phone, a small laptop window and a maximised desktop. It prints the absolute path of every
image so an agent can open them directly.

**Some states exist on one class of device and are captured only there**, and the run says so by
name. The reading column at each end of its range is DESKTOP ONLY, because `@media (min-width:
900px) and (pointer: fine)` is what puts a column on the page at all — on a phone there is no
column, no handle and nothing to photograph. The armed pull-to-refresh is PHONES ONLY for the
mirror-image reason: `has_touch` is set per profile, and a desktop context cannot have the
gesture. A scene that ran on the wrong device and passed would be filed as evidence of an
interface that device does not have, which is why `Scene.profiles` exists and why `--self-test`
checks the restrictions are still on the scenes that need them.

**Dark is the default, and that is not a preference.** This page is used on a phone in a car,
where the scheme is usually dark; the first run of this harness captured nothing but light frames,
because light is the browser-automation default — so every image was real and every one was of a
page nobody actually looks at. Contrast between the two speakers is exactly what does not survive
the swap. `--theme light` or `--theme both` when you want
the other one.

It costs nothing and needs no hardware. For the **live-call** states the conversation WebSocket is
replaced before any page script runs, and the mint request to `/api/v1/signed-url` is answered
locally, so those are reached without ElevenLabs being contacted. The microphone is Chromium's own
fake capture device, so `getUserMedia`, the AudioContext and the real capture graph all still run.

For the two **summary** states there is no page-local stub at all, and there cannot be: summaries
come from a conversation with the ElevenLabs agent and from nothing else, so a harness with no
vendor behind it cannot photograph a summarised channel. `scripts/run.sh --screenshots` therefore
starts `vibe-talk-mock-elevenlabs` — this repository's own loopback vendor, a real mint endpoint
and a real conversation socket — on the two ports above the server's, points the throwaway
config's `elevenlabs.api_base` at it, and passes the harness `--mock-control` so the states can
drive it. One asks for a working summariser and photographs the summarised channel; the other
wedges the same mock so it answers nothing, and photographs the **red** failed row. Those two
frames are therefore evidence about the real socket client and the real cache, not about a stub.
That is most of the remaining half of `#57 elevenlabs-mock`; the live-call states are what is
left. Both processes are stopped by the same trap, including on failure.

It stands up its own throwaway native server with `--fake-discord` on port 18091 and stops it
again, including on failure — it refuses port 8080 by name, because that is the live deployment,
and it refuses a `--port` that would put the mock on 8080 or on the CI container's 18081.

The two ways a screenshot harness lies are both closed, and both were verified by mutation:

* **Thirteen pictures of the same idle screen.** Every state declares what must be true of the live
  page before the shutter opens, and each is pinned to its OWN marker — the post-call shot checks
  for the seam labelled "new conversation", the armed shot for the word the control changes to. A
  state whose expectations do not hold fails BY NAME and is not photographed approximately. A
  further control rejects any expectation still driving a selector the interface rework retired,
  because a stale selector does not fail loudly: `getElementById` returns null and the run blames
  the page.
* **A picture of a page that never rendered.** A white rectangle is a valid PNG. Every capture is
  decoded (in the standard library — no image dependency) and rejected if it is too small, too
  few bytes, under sixteen distinct colours, or more than 99.5% one flat colour. The check sits on
  the only function that writes a file, so there is no second path that saves an unjudged image.

Some expectations are load-bearing beyond "did we get here". The opened seam must land ABOVE the
dock — its first capture caught it unfolding underneath, where it is invisible — so that state
asserts the explanation's bounding box clears the dock's top edge. That assertion was verified end
to end by reintroducing the bug in the browser and confirming the run refused.

`12-collapsed-long-transcript` is the other one, and it is here because a LINE CLAMP CANNOT BE
TESTED ANYWHERE ELSE. The page suite can check that `-webkit-line-clamp: 3` is declared on
`.body.clamped` and that the page puts that class on the right element; whether three lines is
really three lines is a rendering fact, and this is the only thing in the repository that measures
it. It measures the SAME message closed and then open — comparing the first open message against
the first closed one compares two lengths rather than two states, and passed for the wrong reason
at desktop width until it was fixed.

**The walk is a chain, and where that is a lie it has been made false.** Most of these states are
one continuous session on purpose — `04-muted` is a picture of the call `03-live-call` started, and
rebuilding it would be a fiction. What is not allowed is a state that inherits something it does
not photograph: `13-jump-to-newest` used to show a "Collapse all" chip opened by the scene before
it, while its own description says "the chip appeared", singular — and neither it nor
`12-collapsed-long-transcript` could be run with `--only`, because "there is already a folded
transcript on screen" was a requirement nothing stated. The three transcript states now build the
list they photograph, through one shared act, and `--self-test` checks that they still do.
`#74 scroll-test-strength`.

`scripts/screenshots.py --self-test` runs 52 controls for those checks offline, with no browser and
no server; `scripts/test-run-sh.sh` runs them as part of its own suite. Screenshots are written to
the gitignored `debug/screenshots/` and are never committed. Playwright and its Chromium are the
only requirement, and a missing one fails by name with the install command.

The harness is opt-in and in no default suite. It rebuilds the binary first, because `web/` is
compiled into it with `include_str!` — without that you photograph the last build's markup and
believe it is today's.

## Layout

```text
src/config.rs         configuration, environment overrides, secret redaction
src/auth.rs           bearer tokens, read vs write scope
src/access.rs         the access log: what one line says, and what it must never say
src/model.rs          Message/Channel types, snowflake ordering
src/discord/          the DiscordClient trait, the live HTTP client, the in-memory fake
src/discord/ratelimit.rs  Retry-After, the X-RateLimit-* buckets, and the bounded wait-and-retry
src/summary.rs        the extractive digest/preview helper (NOT a summariser: it also flattens
                      the agent's own reply)
src/retrieval.rs      semantic random access, behind the Ranker trait
src/untrusted.rs      the data-not-instructions boundary
src/ops.rs            the operations both front doors share: allowlist, fetch, transform
src/live.rs           bounded polling and adapter push, per-channel fan-out, replay, and SSE bodies
src/replay.rs         rebuilding continuity across a hang-up: the preamble, the budget, the fence
src/probe.rs          the startup channel reachability probe and its failure taxonomy
src/diagnostics.rs    those same checks on demand, structured, redacted, and time-bounded
src/upstream_profile.rs  how long each provider operation and Slack Web API call takes, and how
                      big messages are: numbers only, for diagnostics and one line an hour
src/elevenlabs/       the SignedUrlProvider trait, the live client, the in-memory fake
src/elevenlabs/mock/  the loopback vendor: a real mint endpoint, a real WebSocket, a real MCP client
src/bin/mock_elevenlabs.rs     that mock as a process, for a browser or the smoke script
src/mcp/mod.rs        the tool manifest and per-tool approval policy
src/mcp/protocol.rs   JSON-RPC 2.0 and the MCP method set
src/mcp/transport.rs  the Streamable HTTP endpoint at /mcp
src/store/            the StateStore trait, the SQLite backend, the fake, the refusing one
src/summarize/        the Summarizer trait, the ElevenLabs agent summariser, the counting fake,
                      the cache key
src/agent_backend.rs  the slow-path seam
src/http/             router, handlers, and the access-log middleware
web/                  the phone app and the /voice page (plain HTML/CSS/JS, no framework, no build step)
tests/js/             the /voice page's own suite, run from cargo test via tests/voice_page.rs
scripts/verify-deployment.sh   the one-command deployment check, local or public
scripts/smoke-agent.py         the manual, billed check that the AGENT really calls us
scripts/screenshots.py         photographs /voice in every state, so an agent can SEE the page
QUICKSTART.md         the six-step setup path, start to first conversation
```

### Why your own messages need a setting

Two accounts carry your words in a channel: the one this bridge posts as, and the one you type
into Discord with yourself. The channel view draws both as yours.

The first is free. A Discord bot token's first segment IS the bot's user id, so the server reads it
out of its own token and tells the page before the first message is drawn.

**The second cannot be derived from anything this server holds.** A bot's account has no
relationship to the human reading the channel — the token says what the bot is, not who you are.
So it is `discord.owner_user_id`, and until you set it your own messages come through as somebody
else's.

You can also set it without a restart: **Settings lists every account it has seen** and lets you
say which is which. That is the faster fix, and it is per-browser; the configuration setting is for
a deployment that would rather state it once.

## Running the tests

```bash
cd vibe-talk && make validate        # fmt, clippy/mypy, suites, DAG and Android evaluator controls
cd vibe-talk && make page            # just the page suites — seconds
cd vibe-talk && make validate-boxed  # adds PWA/offline-cache/screenshot browser checks under dagrun
```

`make validate` needs Rust, Node, and Python with mypy, PyYAML, Playwright, and websockets. The
Python packages type-check the operator and browser harness scripts; no browser binary, phone, or
external service is needed by the ordinary gate. The boxed gate additionally needs Chromium for
its PWA, offline-cache, and screenshot checks.

**Prefer `validate-boxed` locally.** It runs the same suites through `dagrun`, which gives each
step a wall and CPU timeout, a memory cap, and process-tree teardown inside a cgroup. That last
part is not a nicety: the screenshot harness starts a real server and a real browser, and the
teardown is setsid-proof — they are reaped with the step whether or not the script's own trap
fired. One that was not once ran for six days holding a port. The graph also caps `browser: 1`,
so two Chromiums can never run at once however many steps ask for one.

The graph is `vibe-talk/tests.dag.yaml`. `make dag-check` loads it without running anything.

## Known gaps

Beyond the security list above:

* **`GET /api/v1/diagnostics` has met no real vendor.** Both halves of it — the Discord checks and
  the ElevenLabs checks — have been exercised only against in-memory fakes. The classification it
  reports is the startup probe's, which is in exactly the same position; what a live vendor
  actually returns for each of those cases is still unobserved.
* `resolve` only searches the window it just fetched (default 50, max 100 messages). Older messages
  are unreachable; there is no pagination and no store.
* Ranking is lexical. It matches words, not meaning, so a paraphrase with no shared words will miss.
  The `Ranker` trait is the replacement point.
* **The only summariser has never met live ElevenLabs, and it is now the default path.** Every
  per-message summary is a real conversation with the configured agent; the extractive truncating
  fallback is gone. What has been exercised is the socket client against this repository's own
  mock, written from the same reading of the vendor's SDK — so the two agree with each other and
  not yet with the vendor. Until somebody runs it for real, the summary feature is unproven on
  every deployment, not just on the ones that opted in.
* **A deployment with no ElevenLabs credentials shows every long row as failed.** It starts, it
  warns, and it says so on the page — but the cache stays empty forever and there is no reduced
  service to fall back on. `summaries.model` is still recorded in the cache key and read by
  nothing, which is the last vestige of a selector that no longer exists.
* No caching of channel content on disk. Two short-lived copies are kept in memory for reading:
  read-aloud preparation reuses messages the page fetched in the last 30 seconds
  (`#191 read-aloud-first-lag`), and the voice catch-up answers from each channel's newest window
  when the provider is slow (`#210 voice-catchup-speed`; see "The eight tools"). Obeying `Retry-After`
  does not change that — it buys time, not quota, so a poll interval still spends one request per
  channel whether or not anybody is listening. That is why `discord.live_poll_seconds` still
  defaults to OFF and why an interval under five seconds is still refused outright rather than
  clamped. The summary cache is the one exception and it is a cache of DERIVED text, bounded and
  purgeable — see "Durable state". The two copies in memory named above are not the only channel
  text held there, and the storage purge clears only those two; "How an operator purges it" lists
  what it leaves.
* **`Retry-After` is handled; the status code and the absence of a queue are what is left.** A 429
  is parsed (body `retry_after` in fractional seconds, the `global` flag, and the `X-RateLimit-*`
  headers), waited out and retried, bounded at four attempts and thirty seconds; a bucket known to
  be empty is not spent; a global limit stops every channel rather than one. See "Discord rate
  limits". Two things are deliberately NOT done: **the exhausted case still reaches an API caller
  as HTTP 502**, not as a 429 with a `Retry-After` of its own, because the error-to-status mapping
  in `src/http/api.rs` was left alone; and **there is no queue** — a request that runs out of
  budget is over, not deferred to be sent later.
* **Live push has never run against live Discord either, and neither has the poller, and neither
  has the rate-limit handling.** The seeding rule, the cursor, the failure path, the SSE framing
  and every wait described under "Discord rate limits" are tested against the in-memory fake on a
  paused clock; the first real deployment with an interval set will be the first time any of it
  meets Discord's rate limiter.
* **Whether the vendor honours a replayed transcript is UNVERIFIED.** `#46 conversation-replay`
  is complete on this side — the budget, the fencing, the four honesty states and the transport
  switch are all tested — but no billed run has yet confirmed that ElevenLabs puts the payload in
  context for the first agent turn. `scripts/run.sh --smoke-agent --replay-check` is the check;
  it has never been run against a live agent. That is why `replay.enabled` defaults to false.
* **The replay budget is a guess.** 6000 characters and 40 turns were chosen, not measured. The
  payload is billed per call.
* **A contextual update reaches the agent only while `/voice` is open.** The conversation socket
  belongs to the browser, not to this server (see "Live push"), so closing the tab ends the relay
  even though the server keeps ingesting.
* The web app has no service worker and no offline mode. It keeps a bounded snapshot of recent
  channel rows in `localStorage` (see "Saved messages on this device"), but the page itself
  cannot load without the server.
* **The MCP endpoint has never been driven by a real ElevenLabs agent.** It is tested against the
  protocol as written and against `curl`; the registration steps above are from the vendor's
  documentation, not from a completed round trip.
* **The Discord layer has still never run against live Discord.** Adding MCP did not change that:
  both front doors go through the same untested-against-production client, and
  `scripts/verify-deployment.sh` has therefore only ever been exercised against the in-memory
  fake — in CI and by hand. Its checks are protocol-level and transport-level, so they apply
  unchanged to a real deployment, but the first real run will also be the script's first real run.
* **Signed URLs have never been minted from live ElevenLabs.** The endpoint, the header, and the
  response shape come from the vendor's current documentation; everything below that is tested
  against an in-memory ElevenLabs that can refuse, and against a loopback HTTP server that proves
  the key is sent as a header. The first live call will still be the first live call.
* The `/voice` page decodes PCM only. If an agent is configured to emit µ-law or MP3, the page says
  so and stops rather than playing noise — it does not transcode.
* **Press-and-hold on the talk control does not offer hang-up**, and `#43 typed-input` asks for it
  as a possibility rather than a requirement: it is worth doing only if a true *pause* exists, and
  whether one does is still open — see `#40`. Mute is what stands in for a pause today, and hang-up
  is its own control in the pane. Deferred deliberately, not dropped.
* **Whether a typed `user_message` is echoed back as a `user_transcript` has never been observed.**
  The `/voice` page assumes it may be and suppresses a matching transcript for ten seconds; one
  billed `--smoke-agent` run would settle it, and that run has not been made.
* **Whether ElevenLabs implements `contextual_update` at all has never been observed**, and two
  features now depend on it: `#46 conversation-replay`'s default transport, and `#73
  mute-is-invisible`'s announcement that a mute is deliberate. The page's half is tested offline
  against the mock vendor, which *models* the event — the model being this repository's belief
  about the contract, written where a test can state it, and not evidence of anything. Whether a
  real agent accepts the frame, and whether reading it actually stops it asking "are you there?",
  takes one billed `--smoke-agent` run that has not been made.
* The `/voice` page captures the microphone through a `ScriptProcessorNode`, which is deprecated
  (though universally supported). Moving it to an `AudioWorklet` would mean a second asset for no
  behavioural gain today.
* The legacy HTTP+SSE MCP transport is not implemented. A client that cannot do Streamable HTTP
  cannot connect.
* No MCP resources, prompts, sampling, or logging capability — tools only.
* No `Mcp-Session-Id`, therefore no server-initiated notifications and no `tools/list_changed`.
  A configuration change is picked up by restarting, and the client re-lists on its next connect.
* This crate is intentionally outside the repository's Rust workspace and is not part of
  `make check` / `make test`; it has its own CI workflow.
* **A direct Discord bot still cannot reach Discord's read state.** Discord shares it with
  clients, not with bots — no ack route, no read-state field on the channel object, no
  `read_state` in the gateway READY payload. The evidence and code anchors are in
  `ai_docs/UNREAD_STATUS_20260819.md`. A compatible provider bridge may expose the separately
  configured `/upstream-read` capability, but that does not turn vibe-talk's own `/read` record or
  its Done/archive state into provider state.

## License

MIT, as with the rest of the repository.
